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

use super::super::git::{
  Authenticator, GitServices, GitSite, LfsService, Located, Proven, Repositories, ServiceRefused,
  Start as GitStart, Unproven,
};
use slates_svn::dav::site::{self, Answer as SiteAnswer, ReportProgress};

use super::{Lent, SvnExchange, SvnSite, SvnStores, served_of, xml_bounds};
use crate::vcs_http::{
  Answer, Bounds, ClientGone, Decided, EdgeState, Exchange, Fields, Site, Status, ToolExchange,
  ToolSites, serve,
};
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
/// Shape: the id of the Mercurial repository `hg`.
const HG: RepositoryId = RepositoryId { bytes: [4; 16] };
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
      "hg" => (HG, VcsTool::Mercurial, ObjectFormat::Sha1),
      _ => return None,
    };
    Some(RepositoryRecord {
      index_bits: 0,
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
        hg_publishing: true,
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

/// A property list: each property's name and value.
type Properties = Vec<(Vec<u8>, Vec<u8>)>;

/// The tests' repository: revision 1 by alice adds `/trunk/a.txt` (with `svn:eol-style`) and `/trunk/secret/s.txt`.
struct Repo {
  /// The index.
  index: Index,
  /// Each revision's tree: each path and whether it is a directory.
  trees: Vec<BTreeMap<String, bool>>,
  /// The property lists, by identity.
  lists: BTreeMap<ContentIdentity, Properties>,
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

  fn text(&self, identity: &ContentIdentity, offset: u64) -> Option<std::borrow::Cow<'_, [u8]>> {
    let text = self.texts.get(identity)?;
    let start = usize::try_from(offset).ok()?;
    text
      .get(start..start.saturating_add(PIECE).min(text.len()))
      .map(std::borrow::Cow::Borrowed)
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
  type Repository<'a> = Repo;
  type Hold = RepositoryId;

  fn hold(&mut self, id: RepositoryId) -> Option<RepositoryId> {
    self.holds(id).then_some(id)
  }

  fn lend_held<T>(
    &mut self,
    hold: &RepositoryId,
    principal: &Principal,
    step: impl FnOnce(&Lent<'_, Repo>) -> T,
  ) -> Option<T> {
    self.lend(*hold, principal, step)
  }

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

/// Makes Subversion repository [`COMMITTED`] on a store-backed daemon's shard and lands its revision 1 through the
/// owner's step (A-655 part 2b; C2), from a history running as a task there: `/trunk`, `/trunk/a.txt`, `/trunk/sub` and
/// `/trunk/sub/b.txt`, by alice. Its id.
async fn committed_repository() -> RepositoryId {
  let asked = (
    COMMITTED,
    slates_ipc::protocol::VcsTool::Subversion,
    slates_ipc::protocol::ObjectFormat::Revisions,
    "main",
  );
  let (id, _) = crate::test_store::created(&daemon_owner(), asked).await;
  let committed =
    crate::repository::svn_commit::tests::commit((&daemon_owner(), id), first_revision(), None)
      .await;
  assert!(committed.is_ok(), "{committed:?}");
  id
}

/// [`committed_repository`]'s revision 1.
fn first_revision() -> slates_svn::commit::revision::Revision {
  let (a, b): (&[u8], &[u8]) = (b"alpha\n", b"beta\n");
  slates_svn::commit::revision::Revision {
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
    stored: BTreeMap::new(),
    lock_entries: Vec::new(),
  }
}

/// What the site reads of [`COMMITTED`] through the daemon's lender, from a task on its shard: whether the lender holds
/// it and another repository; the PROPFIND of `/trunk` at depth 1; the text the GET of `/trunk/sub/b.txt` streams; the
/// `log-report` of every revision.
async fn read_through_the_lender(id: RepositoryId) -> (bool, bool, Vec<u8>, Vec<u8>, Vec<u8>) {
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
      label: None,
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
  let mut fetched_piece = None;
  let fetched = match stores
    .lend(id, &owner, |lent| {
      slates_svn::reads::Repository::text(lent.repository, &text.identity, 0)
        .map(std::borrow::Cow::into_owned)
    })
    .flatten()
  {
    Some(bytes) => bytes,
    None => {
      fetched_piece = stores
        .fetch(
          id,
          slates_svn::reads::Wanted {
            text: text.identity,
            offset: 0,
          },
        )
        .await;
      assert!(fetched_piece.is_some(), "the stored text's piece");
      stores
        .lend(id, &owner, |lent| {
          slates_svn::reads::Repository::text(lent.repository, &text.identity, 0)
            .map(std::borrow::Cow::into_owned)
        })
        .flatten()
        .expect("the text after fetch")
    }
  };
  drop(fetched_piece);
  let mut log = Vec::new();
  while stores
    .lend(id, &owner, |lent| {
      report.resume(
        &served_of(lent, &root),
        &mut log,
        super::slice_budget(room, None),
      )
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
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let id = crate::test_store::run_on_first(
    &daemon,
    committed_repository,
    std::time::Duration::from_secs(60),
  );
  let (sent, outcome) = std::sync::mpsc::sync_channel(1);
  crate::daemon::observe_first(&daemon, move |_state| {
    let task = slates_rt::futures::spawn(async move {
      let _ = sent.send(read_through_the_lender(id).await);
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

/// C2 stage B part 2 (a DAV GET reads a text RAM does not hold through the node's changeset store, R6). Do: on a
/// daemon whose repository `stored` has a revision 1 adding `/trunk/a.bin`, every hold RAM has on its text let go
/// (`crate::test_store::a_text_only_the_store_holds`), GET `/stored/trunk/a.bin` as its owner through the daemon's own
/// lender and finder (`ServedRecords`, `ShardRepositories`), on its shard. Expect `200` and the text whole, read
/// through the store a piece at a time (`vcs_store.svn_texts.piece_fetched` at least once), every fetched piece let go
/// after. Before, the reply's first piece RAM did not hold failed the GET `503`.
#[test]
fn a_get_reads_a_text_ram_does_not_hold_through_the_store() {
  let bytes = crate::test_store::noise(200 << 10);
  let (_store, daemon, _) =
    crate::test_store::a_text_only_the_store_holds(&principal(OWNER), "stored", &bytes);
  let (status, reply) = crate::test_store::run_on_first(
    &daemon,
    || async {
      let mut site = SvnSite::new(
        REALM.to_owned(),
        ROOM,
        Tokens,
        super::super::daemon_edge::ShardRepositories,
        ServedRecords,
      );
      let owner = basic("alice", OWNER_TOKEN);
      let asked = request("GET", "/stored/trunk/a.bin", &[owner.as_str()]);
      let Answer::Serve { mut exchange, .. } = site.answer(&head(&asked)).await.answer else {
        panic!("the GET not served");
      };
      exchange.body_end(ClientGone::never()).await.unwrap();
      let status = exchange.reply_status().code;
      let mut reply = Vec::new();
      while exchange
        .reply(&mut reply, ClientGone::never())
        .await
        .unwrap()
      {}
      (status, reply)
    },
    std::time::Duration::from_secs(60),
  );
  let (fetched, left) = crate::daemon::observe_first(&daemon, |state| {
    (
      state
        .refusals
        .get(crate::vcs_store::svn_texts::PIECE_FETCHED)
        .copied()
        .unwrap_or(0),
      state.vcs_stores.svn_pieces_held(),
    )
  });
  daemon.stop();
  assert_eq!(status, 200);
  assert!(
    reply == bytes,
    "{} bytes answered of {}",
    reply.len(),
    bytes.len()
  );
  assert!(fetched >= 1, "no piece read through the store");
  let pieces = u64::try_from(bytes.len().div_ceil(slates_store::store::PIECE_BYTES)).unwrap();
  assert!(
    fetched <= pieces,
    "{fetched} pieces fetched through the store for a text of {pieces}"
  );
  assert_eq!(left, 0, "a fetched piece was kept");
}

/// Format: a `replay-report` in the 1.8 syntax, as ra_serf's `svnsync synchronize` sends it to `!svn/rev/REV`: the
/// root, every revision since 0, with deltas (`crates/svn/src/dav/fixtures/replay-report.xml`).
const REPLAY_1_8: &[u8] = b"<S:replay-report xmlns:S=\"svn:\"><S:include-path></S:include-path>\
<S:low-water-mark>0</S:low-water-mark><S:send-deltas>1</S:send-deltas></S:replay-report>";

/// Goal 93's svnsync over HTTP (A-876; 28.1 pieces fetched a revision in main's r70): a replay report reads a stored
/// text through the node's store a piece at a time, each piece fetched once. Do: on a daemon whose repository `stored`
/// has a revision 1 adding `/trunk/a.bin`, every hold RAM has on its text let go
/// (`crate::test_store::a_text_only_the_store_holds`), REPORT [`REPLAY_1_8`] at `/stored/!svn/rev/1`, as ra_serf's
/// `svnsync synchronize` sends it, as its owner through the daemon's own lender and finder, on its shard. Expect `200`
/// and the report whole (its `editor-report` closed), the text read through the store
/// (`vcs_store.svn_texts.piece_fetched` at least once) at most once a piece, and every fetched piece let go after.
#[test]
fn a_replay_report_fetches_each_piece_of_a_stored_text_once() {
  let bytes = crate::test_store::noise(200 << 10);
  let (_store, daemon, _) =
    crate::test_store::a_text_only_the_store_holds(&principal(OWNER), "stored", &bytes);
  let (status, reply) = crate::test_store::run_on_first(
    &daemon,
    || async {
      let mut site = SvnSite::new(
        REALM.to_owned(),
        ROOM,
        Tokens,
        super::super::daemon_edge::ShardRepositories,
        ServedRecords,
      );
      let owner = basic("alice", OWNER_TOKEN);
      let asked = request("REPORT", "/stored/!svn/rev/1", &[owner.as_str()]);
      let Answer::Serve { mut exchange, .. } = site.answer(&head(&asked)).await.answer else {
        panic!("the REPORT not served");
      };
      exchange.body(REPLAY_1_8).await.unwrap();
      exchange.body_end(ClientGone::never()).await.unwrap();
      let status = exchange.reply_status().code;
      let mut reply = Vec::new();
      while exchange
        .reply(&mut reply, ClientGone::never())
        .await
        .unwrap()
      {}
      (status, reply)
    },
    std::time::Duration::from_secs(60),
  );
  let (fetched, left) = crate::daemon::observe_first(&daemon, |state| {
    (
      state
        .refusals
        .get(crate::vcs_store::svn_texts::PIECE_FETCHED)
        .copied()
        .unwrap_or(0),
      state.vcs_stores.svn_pieces_held(),
    )
  });
  daemon.stop();
  let tail = String::from_utf8_lossy(
    reply
      .get(reply.len().saturating_sub(300)..)
      .unwrap_or_default(),
  )
  .into_owned();
  assert_eq!(status, 200, "{tail}");
  assert!(reply.ends_with(b"</S:editor-report>\n"), "{tail}");
  assert!(fetched >= 1, "no piece read through the store");
  let pieces = u64::try_from(bytes.len().div_ceil(slates_store::store::PIECE_BYTES)).unwrap();
  assert!(
    fetched <= pieces,
    "{fetched} pieces fetched through the store for a text of {pieces}"
  );
  assert_eq!(left, 0, "a fetched piece was kept");
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
  while ready(exchange.reply(&mut reply, crate::vcs_http::ClientGone::never())).unwrap() {
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
    super::dav_request(&head(&input), ROOM)
  };
  let svn = [
    asked("OPTIONS", "/project"),
    asked("PROPFIND", "/project/trunk"),
    asked("REPORT", "/project/!svn/me"),
    asked("GET", "/project/!svn/rvr/3/trunk/a.txt"),
    asked("POST", "/project/!svn/me"),
    asked("POST", "/project/%21svn/me"),
    asked("POST", "/project/%21%73%76%6e/me"),
    asked("GET", "/project/%21svn/rvr/3/trunk/a.txt"),
    asked("MERGE", "/project/trunk"),
  ];
  let others = [
    asked("GET", "/project/info/refs?service=git-upload-pack"),
    asked("POST", "/project/git-upload-pack"),
    asked("POST", "/project/git-receive-pack"),
    asked("POST", "/project.git/info/lfs/objects/batch"),
    asked("GET", "/project/svn/trunk"),
    asked("POST", "/project/%21Svn/me"),
    asked("POST", "/project/%21svn%2fme"),
  ];
  assert_eq!((svn, others), ([true; 9], [false; 7]));
}

/// Minimal stub site that marks which protocol site [`ToolSites`] dispatched to.
struct StubSite(u16);

struct StubExchange(u16);

impl Exchange for StubExchange {
  async fn body(&mut self, _run: &[u8]) -> Result<(), crate::vcs_http::Failed> {
    Ok(())
  }
  async fn body_end(&mut self, _gone: ClientGone<'_>) -> Result<(), crate::vcs_http::Failed> {
    Ok(())
  }
  async fn reply_fields(&mut self) -> Result<Fields, crate::vcs_http::Failed> {
    Ok(Vec::new())
  }
  fn reply_status(&self) -> Status {
    Status {
      code: self.0,
      reason: "Stub",
    }
  }
  async fn reply(
    &mut self,
    _out: &mut Vec<u8>,
    _gone: ClientGone<'_>,
  ) -> Result<bool, crate::vcs_http::Failed> {
    Ok(false)
  }
}

impl Site for StubSite {
  type Exchange = StubExchange;
  async fn decide(&mut self, _head: &Head<'_>, _secure: bool) -> Decided<Self::Exchange> {
    Decided {
      answer: Answer::Refuse {
        status: Status {
          code: self.0,
          reason: "Stub",
        },
        fields: Vec::new(),
      },
      proven: false,
    }
  }
}

type TestToolSites = ToolSites<StubSite, StubSite, SvnSite<Tokens, Held, Stores>>;

fn assert_tool_sites_svn_get(tools: &mut TestToolSites, owner: &str) {
  let get_req = request("GET", "/repo/trunk/a.txt", &[owner]);
  let Answer::Serve {
    mut exchange,
    head_only,
    ..
  } = ready(tools.decide(&head(&get_req), false)).answer
  else {
    panic!("authenticated GET /repo/trunk/a.txt must be served by SVN");
  };
  assert!(!head_only);
  assert!(matches!(exchange, ToolExchange::Svn(_)));
  ready(exchange.body_end(ClientGone::never())).unwrap();
  assert_eq!(exchange.reply_status().code, 200);
  let _ = ready(exchange.reply_fields()).unwrap();
  let mut body = Vec::new();
  while ready(exchange.reply(&mut body, ClientGone::never())).unwrap() {}
  assert_eq!(body.as_slice(), A_TEXT);
}

fn assert_tool_sites_svn_head(tools: &mut TestToolSites, owner: &str) {
  let head_req = request("HEAD", "/repo/trunk/a.txt", &[owner]);
  let Answer::Serve {
    mut exchange,
    head_only,
    ..
  } = ready(tools.decide(&head(&head_req), false)).answer
  else {
    panic!("authenticated HEAD /repo/trunk/a.txt must be served by SVN");
  };
  assert!(head_only);
  assert!(matches!(exchange, ToolExchange::Svn(_)));
  ready(exchange.body_end(ClientGone::never())).unwrap();
  assert_eq!(exchange.reply_status().code, 200);
}

fn assert_tool_sites_svn_auth_and_missing(tools: &mut TestToolSites, owner: &str, stranger: &str) {
  let unauth_req = request("GET", "/repo/trunk/a.txt", &[]);
  let Answer::Refuse { status, fields } = ready(tools.decide(&head(&unauth_req), false)).answer
  else {
    panic!("unauthenticated GET must be refused 401");
  };
  assert_eq!(status.code, 401);
  assert!(
    fields
      .iter()
      .any(|(name, _)| name.eq_ignore_ascii_case(b"WWW-Authenticate")),
    "401 must carry WWW-Authenticate: {fields:?}"
  );

  let forbidden_req = request("GET", "/repo/trunk/a.txt", &[stranger]);
  let Answer::Refuse { status, .. } = ready(tools.decide(&head(&forbidden_req), false)).answer
  else {
    panic!("unshared principal GET must be refused 403");
  };
  assert_eq!(status.code, 403);

  let missing_req = request("GET", "/repo/trunk/no-such.txt", &[owner]);
  let Answer::Serve { mut exchange, .. } = ready(tools.decide(&head(&missing_req), false)).answer
  else {
    panic!("missing file GET must reach SVN exchange and answer 404");
  };
  ready(exchange.body_end(ClientGone::never())).unwrap();
  assert_eq!(exchange.reply_status().code, 404);
}

fn assert_tool_sites_move_and_git_repo_isolation(tools: &mut TestToolSites, owner: &str) {
  let away_req = request("GET", "/away/trunk/a.txt", &[owner]);
  assert!(matches!(
    ready(tools.decide(&head(&away_req), false)).answer,
    Answer::Move { shard } if shard == AWAY.1
  ));

  let git_as_svn = request("GET", "/r.git/trunk/a.txt", &[owner]);
  let Answer::Refuse { status, .. } = ready(tools.decide(&head(&git_as_svn), false)).answer else {
    panic!("GET on a git repository path must dispatch to the Git site, never SvnSite");
  };
  assert_eq!(status.code, 418);
}

fn assert_tool_sites_stub_status(
  tools: &mut TestToolSites,
  method: &str,
  target: &str,
  owner: &str,
  expected_code: u16,
) {
  let req = request(method, target, &[owner]);
  assert!(matches!(
    ready(tools.decide(&head(&req), false)).answer,
    Answer::Refuse { status, .. } if status.code == expected_code
  ));
}

fn assert_tool_sites_protocol_isolation(tools: &mut TestToolSites, owner: &str) {
  assert_tool_sites_stub_status(
    tools,
    "GET",
    "/r.git/info/refs?service=git-upload-pack",
    owner,
    418,
  );
  assert_tool_sites_stub_status(tools, "GET", "/r.git/HEAD", owner, 418);
  assert_tool_sites_stub_status(
    tools,
    "PUT",
    "/r.git/info/lfs/objects/0000000000000000000000000000000000000000000000000000000000000000",
    owner,
    418,
  );
  assert_tool_sites_stub_status(tools, "GET", "/hg?cmd=capabilities", owner, 421);
}

/// SVN-5: Do send public `GET` and `HEAD` requests (no `!svn/` segment) through the full
/// [`crate::vcs_http::ToolSites`] router wrapping Git, Hg, and SVN sites. Expect:
/// - authenticated `GET /repo/trunk/a.txt` -> `200` with exact bytes `A_TEXT`
/// - authenticated `HEAD /repo/trunk/a.txt` -> `200` with `head_only == true`
/// - unauthenticated `GET /repo/trunk/a.txt` -> `401` with `WWW-Authenticate`
/// - unshared principal `GET /repo/trunk/a.txt` -> `403`
/// - missing file `GET /repo/trunk/missing.txt` -> `404`
/// - cross-shard `GET /away/trunk/a.txt` -> `Answer::Move { shard: AWAY.1 }`
/// - cross-protocol isolation: `GET /r.git/trunk/a.txt` -> routed to Git site (never served as SVN),
///   `GET /r.git/info/refs?service=git-upload-pack` -> routed to Git site,
///   `GET /r.git/HEAD` -> routed to Git site (dumb protocol `403`),
///   `GET /hg?cmd=capabilities` -> routed to Hg site,
///   `PUT /r.git/info/lfs/objects/0000000000000000000000000000000000000000000000000000000000000000` -> routed to Git site.
#[test]
fn tool_sites_routes_public_get_and_head_to_svn_preserving_auth_move_and_protocol_isolation() {
  // Distinct sentinel statuses for Git (418) and Hg (421) stub sites so we verify routing cleanly.
  let mut tools = ToolSites::new(StubSite(418), StubSite(421), site(), ROOM);
  let owner = basic("alice", OWNER_TOKEN);
  let stranger = basic("bob", STRANGER_TOKEN);

  assert_tool_sites_svn_get(&mut tools, &owner);
  assert_tool_sites_svn_head(&mut tools, &owner);
  assert_tool_sites_svn_auth_and_missing(&mut tools, &owner, &stranger);
  assert_tool_sites_move_and_git_repo_isolation(&mut tools, &owner);
  assert_tool_sites_protocol_isolation(&mut tools, &owner);
}

/// Minimal Git service exchange for testing real `GitSite` inside [`ToolSites`].
struct MockGitExchange;

impl Exchange for MockGitExchange {
  async fn body(&mut self, _run: &[u8]) -> Result<(), crate::vcs_http::Failed> {
    Ok(())
  }
  async fn body_end(&mut self, _gone: ClientGone<'_>) -> Result<(), crate::vcs_http::Failed> {
    Ok(())
  }
  async fn reply(
    &mut self,
    out: &mut Vec<u8>,
    _gone: ClientGone<'_>,
  ) -> Result<bool, crate::vcs_http::Failed> {
    out.extend_from_slice(b"0000");
    Ok(false)
  }
}

/// Minimal `GitServices` serving every Git service for real `GitSite` routing tests.
struct MockGitServices;

impl GitServices for MockGitServices {
  type Exchange = MockGitExchange;
  fn start(&mut self, _start: &GitStart<'_>) -> Result<Self::Exchange, ServiceRefused> {
    Ok(MockGitExchange)
  }
}

/// Minimal `LfsService` serving every LFS endpoint with `200 OK` for real `GitSite` routing tests.
#[derive(Clone, Copy)]
struct MockLfs;

impl LfsService for MockLfs {
  fn serves(&self, _endpoint: &slates_git::lfs::serve::Endpoint) -> bool {
    true
  }
  fn answer(&mut self, _call: &crate::lfs::LfsCall<'_>) -> slates_git::lfs::serve::Answer {
    slates_git::lfs::serve::Answer {
      status: Status::OK,
      kind: slates_git::lfs::serve::Kind::Empty,
      challenge: None,
      body: b"{}".to_vec(),
    }
  }
}

/// Shape: 64-hex LFS OID used in SVN-5 path-suffix collision tests.
const LFS_OID_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000000";
/// Shape: 2+38 hex loose Git object path used in SVN-5 path-suffix collision tests.
const LOOSE_OBJ_PATH: &str = "/trunk/objects/ab/0123456789abcdef0123456789abcdef012345";
/// Shape: SVN file paths colliding with Git smart/dumb HTTP and Git LFS trailing suffixes.
const SVN_SUFFIX_FILES: &[(&str, &[u8])] = &[
  ("/trunk/HEAD", b"svn-head\n"),
  ("/trunk/info/refs", b"svn-info-refs\n"),
  ("/trunk/git-upload-pack", b"svn-upload-pack\n"),
  ("/trunk/git-receive-pack", b"svn-receive-pack\n"),
  ("/trunk/git-upload-archive", b"svn-upload-archive\n"),
  ("/trunk/objects/info/packs", b"svn-packs\n"),
  (LOOSE_OBJ_PATH, b"svn-loose\n"),
  (
    "/trunk/info/lfs/objects/0000000000000000000000000000000000000000000000000000000000000000",
    b"svn-lfs-obj\n",
  ),
  ("/trunk/info/lfs/objects/batch", b"svn-lfs-batch\n"),
];

/// Builds a test SVN [`Repo`] populated with files whose names match Git/LFS trailing path
/// suffixes plus an empty `/trunk/existing` directory (lacking `/trunk/existing/HEAD`).
fn repo_with_git_suffix_files() -> Repo {
  let dirs = [
    "/trunk",
    "/trunk/existing",
    "/trunk/info",
    "/trunk/info/lfs",
    "/trunk/info/lfs/objects",
    "/trunk/objects",
    "/trunk/objects/info",
    "/trunk/objects/ab",
  ];
  let mut changes: Vec<ChangedPath> = dirs
    .iter()
    .map(|path| added(path, NodeKind::Directory, None, None))
    .collect();
  let mut tree_r1 = BTreeMap::from([("/".to_owned(), true)]);
  for dir in dirs {
    tree_r1.insert(dir.to_owned(), true);
  }
  let mut texts = BTreeMap::new();
  for (path, bytes) in SVN_SUFFIX_FILES {
    let text = text_of(bytes);
    texts.insert(text.identity, bytes.to_vec());
    changes.push(added(path, NodeKind::File, Some(text), None));
    tree_r1.insert((*path).to_owned(), false);
  }
  let mut index = Index::new();
  index
    .append(
      RevisionRecord {
        transaction: "0-1".to_owned(),
        changes,
      },
      usize::try_from(ROOM).unwrap(),
    )
    .unwrap();
  let date = |value: &str| (b"svn:date".to_vec(), value.as_bytes().to_vec());
  Repo {
    index,
    trees: vec![BTreeMap::from([("/".to_owned(), true)]), tree_r1],
    lists: BTreeMap::new(),
    texts,
    revisions: vec![
      vec![date("2026-11-18T00:00:00.000000Z")],
      vec![
        (b"svn:author".to_vec(), b"alice".to_vec()),
        date(DATE),
        (b"svn:log".to_vec(), b"git-suffix files".to_vec()),
      ],
    ],
    locks: LockTable::new(),
    authz: Authz::parse(AUTHZ, None, authz::Bounds::of(AUTHZ, None)).unwrap(),
  }
}

type RealToolSites = ToolSites<
  GitSite<Tokens, Held, MockGitServices, MockLfs>,
  StubSite,
  SvnSite<Tokens, Held, Stores>,
>;

fn real_tool_sites_with_suffix_files() -> RealToolSites {
  let repo: &'static Repo = Box::leak(Box::new(repo_with_git_suffix_files()));
  let git = GitSite::new(REALM.to_owned(), ROOM, Tokens, Held, MockGitServices).with_lfs(MockLfs);
  let svn = SvnSite::new(REALM.to_owned(), ROOM, Tokens, Held, Stores { repo });
  ToolSites::new(git, StubSite(421), svn, ROOM)
}

fn assert_svn_get_returns_bytes(
  tools: &mut RealToolSites,
  owner: &str,
  target: &str,
  expected_bytes: &[u8],
) {
  let get_req = request("GET", target, &[owner]);
  let Answer::Serve {
    mut exchange,
    head_only,
    ..
  } = ready(tools.decide(&head(&get_req), false)).answer
  else {
    panic!("GET {target} must be served by SvnSite");
  };
  assert!(!head_only, "GET {target} must not be head_only");
  assert!(matches!(exchange, ToolExchange::Svn(_)));
  ready(exchange.body_end(ClientGone::never())).unwrap();
  assert_eq!(exchange.reply_status().code, 200, "GET {target}");
  let _ = ready(exchange.reply_fields()).unwrap();
  let mut body = Vec::new();
  while ready(exchange.reply(&mut body, ClientGone::never())).unwrap() {}
  assert_eq!(body.as_slice(), expected_bytes, "GET {target} body");
}

fn assert_svn_head_status(
  tools: &mut RealToolSites,
  owner: &str,
  target: &str,
  expected_status: u16,
  serve_reason: &str,
) {
  let head_req = request("HEAD", target, &[owner]);
  let Answer::Serve {
    mut exchange,
    head_only,
    ..
  } = ready(tools.decide(&head(&head_req), false)).answer
  else {
    panic!("{serve_reason}");
  };
  assert!(head_only, "HEAD {target} must be head_only");
  assert!(matches!(exchange, ToolExchange::Svn(_)));
  ready(exchange.body_end(ClientGone::never())).unwrap();
  assert_eq!(
    exchange.reply_status().code,
    expected_status,
    "HEAD {target}"
  );
}

fn assert_svn_dav_routes_to_svn_site(tools: &mut RealToolSites, owner: &str) {
  let trunk_lfs_object = format!("/repo/trunk/info/lfs/objects/{LFS_OID_HEX}");
  let txn_lfs_object = format!("/repo/!svn/txr/1-1/trunk/info/lfs/objects/{LFS_OID_HEX}");
  for (method, target) in [
    ("PROPFIND", "/repo/trunk/HEAD"),
    ("MKCOL", "/repo/trunk/existing/HEAD"),
    ("PUT", trunk_lfs_object.as_str()),
    ("PUT", txn_lfs_object.as_str()),
  ] {
    let req = request(method, target, &[owner]);
    let Answer::Serve { exchange, .. } = ready(tools.decide(&head(&req), false)).answer else {
      panic!("{method} {target} on SVN repo must route to SvnSite");
    };
    assert!(
      matches!(exchange, ToolExchange::Svn(_)),
      "{method} {target} must dispatch to SvnSite"
    );
  }
}

/// SVN-5: Do send authenticated `GET`, `HEAD`, and DAV requests on an SVN repository for files
/// whose names match Git smart/dumb HTTP and Git LFS trailing path suffixes, plus an absent
/// `HEAD /repo/trunk/existing/HEAD` (as `ra_serf` sends before `PUT`) and a `POST` to
/// `/repo/info/lfs/objects/batch`. Expect: every existing SVN file returns `200 OK` with exact
/// bytes on `GET` and `head_only == true` on `HEAD`; absent `HEAD /repo/trunk/existing/HEAD`
/// returns `404` from `SvnSite` (not `403` from `GitSite`); DAV `PUT`/`MKCOL`/`PROPFIND` route to
/// `SvnSite`; and `POST /repo/info/lfs/objects/batch` refuses `404` (never succeeds as LFS).
#[test]
fn svn5_tool_sites_serves_svn_paths_matching_git_and_lfs_suffixes() {
  let mut tools = real_tool_sites_with_suffix_files();
  let owner = basic("alice", OWNER_TOKEN);
  for (path, expected_bytes) in SVN_SUFFIX_FILES {
    let target = format!("/repo{path}");
    assert_svn_get_returns_bytes(&mut tools, &owner, &target, expected_bytes);
    assert_svn_head_status(
      &mut tools,
      &owner,
      &target,
      200,
      &format!("HEAD {target} must be served by SvnSite"),
    );
  }

  // Absent HEAD before DAV PUT (`svn commit` adding `trunk/existing/HEAD` via `ra_serf`).
  assert_svn_head_status(
    &mut tools,
    &owner,
    "/repo/trunk/existing/HEAD",
    404,
    "HEAD /repo/trunk/existing/HEAD must reach SvnSite, not GitSite 403",
  );

  // DAV PUT/MKCOL/PROPFIND on LFS/Git-suffix paths inside an SVN repo must go to SvnSite.
  assert_svn_dav_routes_to_svn_site(&mut tools, &owner);

  // POST LFS batch on an SVN repo must refuse cleanly (404), never succeed as LFS.
  let lfs_batch = request(
    "POST",
    "/repo/info/lfs/objects/batch",
    &[
      &owner,
      "Accept: application/vnd.git-lfs+json",
      "Content-Type: application/vnd.git-lfs+json",
    ],
  );
  let Answer::Refuse { status, .. } = ready(tools.decide(&head(&lfs_batch), false)).answer else {
    panic!("POST /repo/info/lfs/objects/batch on SVN repo must be refused");
  };
  assert_eq!(status.code, 404);
}

fn assert_real_git_http_routing(tools: &mut RealToolSites, owner: &str) {
  let info_refs = request("GET", "/r.git/info/refs?service=git-upload-pack", &[owner]);
  let Answer::Serve { exchange, .. } = ready(tools.decide(&head(&info_refs), false)).answer else {
    panic!("GET /r.git/info/refs must serve via GitSite");
  };
  assert!(matches!(exchange, ToolExchange::Git(_)));

  for method in ["GET", "HEAD"] {
    let dumb = request(method, "/r.git/HEAD", &[owner]);
    let Answer::Refuse { status, .. } = ready(tools.decide(&head(&dumb), false)).answer else {
      panic!("{method} /r.git/HEAD must be refused 403 by GitSite");
    };
    assert_eq!(status.code, 403);
  }

  let plain_git_get = request("GET", "/r.git/trunk/a.txt", &[owner]);
  let Answer::Refuse { status, .. } = ready(tools.decide(&head(&plain_git_get), false)).answer
  else {
    panic!("GET /r.git/trunk/a.txt must be refused 404");
  };
  assert_eq!(status.code, 404);
}

fn assert_git_lfs_serves_200(
  tools: &mut RealToolSites,
  method: &str,
  target: &str,
  headers: &[&str],
) {
  let req = request(method, target, headers);
  let Answer::Serve { mut exchange, .. } = ready(tools.decide(&head(&req), false)).answer else {
    panic!("{method} {target} must serve via GitSite");
  };
  assert!(matches!(exchange, ToolExchange::Git(_)));
  ready(exchange.body_end(ClientGone::never())).unwrap();
  assert_eq!(exchange.reply_status().code, 200);
}

/// SVN-5: Do send Git smart HTTP, dumb HTTP, LFS (batch + basic PUT), and non-Git plain `GET`
/// against a real `VcsTool::Git` repo, plus wire and hgweb requests against `VcsTool::Mercurial`.
/// Expect: Git serves `info/refs` and LFS (`200`), refuses dumb `HEAD` (`403`), and rejects
/// `/r.git/trunk/a.txt` (`404`); Hg routes both `?cmd=` and hgweb to `HgSite`.
#[test]
fn svn5_tool_sites_preserves_real_git_and_hg_routing() {
  let mut tools = real_tool_sites_with_suffix_files();
  let owner = basic("alice", OWNER_TOKEN);

  assert_real_git_http_routing(&mut tools, &owner);

  let lfs_put_target = format!("/r.git/info/lfs/objects/{LFS_OID_HEX}");
  assert_git_lfs_serves_200(&mut tools, "PUT", &lfs_put_target, &[&owner]);
  assert_git_lfs_serves_200(
    &mut tools,
    "POST",
    "/r.git/info/lfs/objects/batch",
    &[
      &owner,
      "Accept: application/vnd.git-lfs+json",
      "Content-Type: application/vnd.git-lfs+json",
    ],
  );

  for target in ["/hg?cmd=capabilities", "/hg"] {
    let req = request("GET", target, &[&owner]);
    assert!(
      matches!(
        ready(tools.decide(&head(&req), false)).answer,
        Answer::Refuse { status, .. } if status.code == 421
      ),
      "GET {target} must route to HgSite"
    );
  }
}

/// SVN-5: Do send authenticated requests for a repository located on another shard (`AWAY`) across
/// SVN suffix paths (`/trunk/HEAD`, `/trunk/existing/HEAD`) and Git/LFS paths (`/info/refs`,
/// `/info/lfs/objects/...`), plus an unauthenticated request to `/away/trunk/HEAD`. Expect: every
/// authenticated request returns `Answer::Move { shard: AWAY.1 }` with `proven == true`, and the
/// unauthenticated request refuses `401` without moving.
#[test]
fn svn5_tool_sites_preserves_cross_shard_move_for_svn_and_git_suffix_paths() {
  let mut tools = real_tool_sites_with_suffix_files();
  let owner = basic("alice", OWNER_TOKEN);
  let lfs_obj = format!("/away/info/lfs/objects/{LFS_OID_HEX}");
  for (method, target) in [
    ("GET", "/away/trunk/HEAD"),
    ("HEAD", "/away/trunk/existing/HEAD"),
    ("GET", "/away/info/refs?service=git-upload-pack"),
    ("PUT", lfs_obj.as_str()),
  ] {
    let req = request(method, target, &[&owner]);
    let decided = ready(tools.decide(&head(&req), false));
    assert!(
      decided.proven && matches!(decided.answer, Answer::Move { shard } if shard == AWAY.1),
      "{method} {target} must Move to shard {} with proven=true",
      AWAY.1
    );
  }

  let unauth = request("GET", "/away/trunk/HEAD", &[]);
  let decided = ready(tools.decide(&head(&unauth), false));
  assert!(!decided.proven);
  assert!(matches!(
    decided.answer,
    Answer::Refuse { status, .. } if status.code == 401
  ));
}

/// SVN-5 (auth-before-existence): Do send unauthenticated and invalid-credential `GET` and `HEAD`
/// requests across nonexistent (`no-such`), private SVN (`repo`), and private Git (`r.git`)
/// repositories for plain paths and Git/LFS suffix paths. Expect: every single request refuses
/// `401 Unauthorized` with `WWW-Authenticate` and `proven == false`, leaking neither repository
/// existence nor registered VCS tool.
#[test]
fn svn5_tool_sites_unauthenticated_get_and_head_never_leak_existence_or_tool() {
  let mut tools = real_tool_sites_with_suffix_files();
  let lfs_obj = format!("/trunk/info/lfs/objects/{LFS_OID_HEX}");
  let paths = [
    "/trunk/HEAD",
    "/HEAD",
    "/trunk/info/refs",
    "/trunk/git-upload-pack",
    "/trunk/objects/info/packs",
    lfs_obj.as_str(),
    "/trunk/info/lfs/objects/batch",
    "/trunk/a.txt",
  ];
  let bad_creds: &[&[&str]] = &[&[], &["Authorization: Bearer invalid-token"]];
  for repo_name in ["no-such", "repo", "r.git"] {
    for suffix in paths {
      let target = format!("/{repo_name}{suffix}");
      for method in ["GET", "HEAD"] {
        for headers in bad_creds {
          let req = request(method, &target, headers);
          let decided = ready(tools.decide(&head(&req), false));
          assert!(!decided.proven, "{method} {target} {headers:?}");
          let Answer::Refuse { status, fields } = decided.answer else {
            panic!("{method} {target} {headers:?} must refuse 401");
          };
          assert_eq!(status.code, 401, "{method} {target} {headers:?}");
          assert!(
            fields
              .iter()
              .any(|(name, _)| name.eq_ignore_ascii_case(b"WWW-Authenticate")),
            "{method} {target} {headers:?} 401 must carry WWW-Authenticate"
          );
        }
      }
    }
  }
}

/// Shape: SVN-planted files under `/app/...` inside SVN repo `"team"` used to test dumb-HTTP
/// forgery defense when Git repo `"team/app"` is also registered.
const SPOOFED_SVN_FILES: &[(&str, &[u8])] = &[
  ("/app/HEAD", b"ref: refs/heads/spoofed\n"),
  (
    "/app/info/refs",
    b"0000000000000000000000000000000000000000\trefs/heads/spoofed\n",
  ),
  ("/app/objects/info/packs", b"P pack-spoofed.pack\n"),
  (
    "/app/info/lfs/objects/0000000000000000000000000000000000000000000000000000000000000000",
    b"spoofed-lfs\n",
  ),
  ("/trunk/hello.txt", b"hello-svn-team\n"),
];

/// Shape: remote shard id hosting `"team/remote-app"` in the multi-segment routing tests.
const REMOTE_APP_SHARD: u16 = 4;

/// Builds a test SVN [`Repo`] representing SVN `"team"` with attacker-planted `/app/HEAD`,
/// `/app/info/refs`, `/app/objects/info/packs`, `/app/info/lfs/objects/<64hex>`, and legitimate
/// `/trunk/hello.txt`.
fn repo_with_spoofed_git_paths() -> Repo {
  let dirs = [
    "/app",
    "/app/info",
    "/app/info/lfs",
    "/app/info/lfs/objects",
    "/app/objects",
    "/app/objects/info",
    "/trunk",
  ];
  let mut changes: Vec<ChangedPath> = dirs
    .iter()
    .map(|path| added(path, NodeKind::Directory, None, None))
    .collect();
  let mut tree_r1 = BTreeMap::from([("/".to_owned(), true)]);
  for dir in dirs {
    tree_r1.insert(dir.to_owned(), true);
  }
  let mut texts = BTreeMap::new();
  for (path, bytes) in SPOOFED_SVN_FILES {
    let text = text_of(bytes);
    texts.insert(text.identity, bytes.to_vec());
    changes.push(added(path, NodeKind::File, Some(text), None));
    tree_r1.insert((*path).to_owned(), false);
  }
  let mut index = Index::new();
  index
    .append(
      RevisionRecord {
        transaction: "0-1".to_owned(),
        changes,
      },
      usize::try_from(ROOM).unwrap(),
    )
    .unwrap();
  let date = |value: &str| (b"svn:date".to_vec(), value.as_bytes().to_vec());
  Repo {
    index,
    trees: vec![BTreeMap::from([("/".to_owned(), true)]), tree_r1],
    lists: BTreeMap::new(),
    texts,
    revisions: vec![
      vec![date("2026-11-18T00:00:00.000000Z")],
      vec![
        (b"svn:author".to_vec(), b"alice".to_vec()),
        date(DATE),
        (b"svn:log".to_vec(), b"spoofed git paths".to_vec()),
      ],
    ],
    locks: LockTable::new(),
    authz: Authz::parse(AUTHZ, None, authz::Bounds::of(AUTHZ, None)).unwrap(),
  }
}

/// Catalog fixture containing overlapping first-segment and multi-segment repositories across
/// SVN, Git, and Hg on a single shard (plus remote `"team/remote-app"` on [`REMOTE_APP_SHARD`]).
struct NestedRepos;

impl Repositories for NestedRepos {
  fn locate(&self, name: &str) -> Located {
    if name == "team/remote-app" {
      Located::Elsewhere(REMOTE_APP_SHARD)
    } else {
      Located::Here
    }
  }

  fn find(&self, name: &str) -> Option<RepositoryRecord> {
    let (id, tool, format) = match name {
      "team" => (REPO, VcsTool::Subversion, ObjectFormat::Revisions),
      "team/app" => (
        RepositoryId { bytes: [11; 16] },
        VcsTool::Git,
        ObjectFormat::Sha1,
      ),
      "hgroot" => (
        RepositoryId { bytes: [12; 16] },
        VcsTool::Mercurial,
        ObjectFormat::Sha1,
      ),
      "hgroot/app" => (
        RepositoryId { bytes: [13; 16] },
        VcsTool::Git,
        ObjectFormat::Sha1,
      ),
      "team/hgsub" => (
        RepositoryId { bytes: [14; 16] },
        VcsTool::Mercurial,
        ObjectFormat::Sha1,
      ),
      "org/hgrepo" => (
        RepositoryId { bytes: [15; 16] },
        VcsTool::Mercurial,
        ObjectFormat::Sha1,
      ),
      _ => return None,
    };
    Some(RepositoryRecord {
      index_bits: 0,
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
        hg_publishing: true,
      },
    })
  }
}

type NestedToolSites = ToolSites<
  GitSite<Tokens, NestedRepos, MockGitServices, MockLfs>,
  StubSite,
  SvnSite<Tokens, NestedRepos, Stores>,
>;

fn nested_tool_sites() -> NestedToolSites {
  let repo: &'static Repo = Box::leak(Box::new(repo_with_spoofed_git_paths()));
  let git =
    GitSite::new(REALM.to_owned(), ROOM, Tokens, NestedRepos, MockGitServices).with_lfs(MockLfs);
  let svn = SvnSite::new(REALM.to_owned(), ROOM, Tokens, NestedRepos, Stores { repo });
  ToolSites::new(git, StubSite(421), svn, ROOM)
}

fn assert_nested_git_serves(
  tools: &mut NestedToolSites,
  method: &str,
  target: &str,
  headers: &[&str],
) {
  let req = request(method, target, headers);
  let Answer::Serve { exchange, .. } = ready(tools.decide(&head(&req), false)).answer else {
    panic!("{method} {target} must serve via GitSite, not SVN/Hg");
  };
  assert!(
    matches!(exchange, ToolExchange::Git(_)),
    "{method} {target} must dispatch to GitSite"
  );
}

fn assert_nested_git_dumb_refused_403(tools: &mut NestedToolSites, target: &str, owner: &str) {
  let req = request("GET", target, &[owner]);
  let Answer::Refuse { status, .. } = ready(tools.decide(&head(&req), false)).answer else {
    panic!("GET {target} must be refused 403 by GitSite, never served from SVN");
  };
  assert_eq!(status.code, 403, "GET {target}");
}

fn assert_nested_hg_and_remote(tools: &mut NestedToolSites, owner: &str) {
  let hg_sub = request("GET", "/team/hgsub?cmd=capabilities", &[owner]);
  assert!(matches!(
    ready(tools.decide(&head(&hg_sub), false)).answer,
    Answer::Refuse { status, .. } if status.code == 421
  ));

  let remote = request(
    "GET",
    "/team/remote-app/info/refs?service=git-upload-pack",
    &[owner],
  );
  assert!(matches!(
    ready(tools.decide(&head(&remote), false)).answer,
    Answer::Move { shard } if shard == REMOTE_APP_SHARD
  ));
}

fn assert_nested_svn_file_200(tools: &mut NestedToolSites, target: &str, owner: &str) {
  let svn_file = request("GET", target, &[owner]);
  let Answer::Serve {
    mut exchange,
    head_only,
    ..
  } = ready(tools.decide(&head(&svn_file), false)).answer
  else {
    panic!("GET {target} must serve via SvnSite");
  };
  assert!(!head_only);
  assert!(matches!(exchange, ToolExchange::Svn(_)));
  ready(exchange.body_end(ClientGone::never())).unwrap();
  assert_eq!(exchange.reply_status().code, 200);
}

/// Blocker 1: Do send Git smart HTTP, dumb HTTP, Git LFS (exact and `.git` alias), and Hg `?cmd=`
/// requests targeting multi-segment repos (`team/app`, `hgroot/app`, `team/hgsub`,
/// `team/remote-app`) while first-segment SVN `team` (holding attacker-planted `app/HEAD`,
/// `app/info/refs`, `app/objects/info/packs`, `app/info/lfs/objects/<64hex>`) and Hg `hgroot`
/// also exist. Expect: every multi-segment Git/LFS request routes to `GitSite` (never shadowed or
/// spoofed by SVN `team` or Hg `hgroot`), `team/hgsub?cmd=capabilities` routes to `HgSite`,
/// `team/remote-app` moves to shard 4, and non-repo `GET /team/trunk/hello.txt` stays on SVN.
#[test]
fn blocker1_multi_segment_git_lfs_and_hg_repos_not_shadowed_or_spoofed() {
  let mut tools = nested_tool_sites();
  let owner = basic("alice", OWNER_TOKEN);
  let lfs_headers = [
    owner.as_str(),
    "Accept: application/vnd.git-lfs+json",
    "Content-Type: application/vnd.git-lfs+json",
  ];
  let team_lfs_put = format!("/team/app/info/lfs/objects/{LFS_OID_HEX}");
  let team_dot_git_lfs_put = format!("/team/app.git/info/lfs/objects/{LFS_OID_HEX}");
  let hgroot_lfs_put = format!("/hgroot/app/info/lfs/objects/{LFS_OID_HEX}");

  assert_nested_git_serves(
    &mut tools,
    "GET",
    "/team/app/info/refs?service=git-upload-pack",
    &[&owner],
  );
  assert_nested_git_serves(&mut tools, "PUT", &team_lfs_put, &[&owner]);
  assert_nested_git_serves(
    &mut tools,
    "POST",
    "/team/app/info/lfs/objects/batch",
    &lfs_headers,
  );
  assert_nested_git_serves(&mut tools, "PUT", &team_dot_git_lfs_put, &[&owner]);
  assert_nested_git_serves(
    &mut tools,
    "POST",
    "/team/app.git/info/lfs/objects/batch",
    &lfs_headers,
  );
  assert_nested_git_serves(
    &mut tools,
    "GET",
    "/hgroot/app/info/refs?service=git-upload-pack",
    &[&owner],
  );
  assert_nested_git_serves(&mut tools, "PUT", &hgroot_lfs_put, &[&owner]);

  assert_nested_git_dumb_refused_403(&mut tools, "/team/app/HEAD", &owner);
  assert_nested_git_dumb_refused_403(&mut tools, "/team/app/info/refs", &owner);
  assert_nested_git_dumb_refused_403(&mut tools, "/team/app/objects/info/packs", &owner);

  assert_nested_hg_and_remote(&mut tools, &owner);
  assert_nested_svn_file_200(&mut tools, "/team/trunk/hello.txt", &owner);
}

fn assert_svn_propfind_207(tools: &mut RealToolSites, target: &str, owner: &str) {
  let propfind = request("PROPFIND", target, &[owner, "Depth: 0"]);
  let Answer::Serve { mut exchange, .. } = ready(tools.decide(&head(&propfind), false)).answer
  else {
    panic!("PROPFIND {target} must route to SvnSite");
  };
  assert!(matches!(exchange, ToolExchange::Svn(_)));
  ready(exchange.body(PROPFIND_KIND)).unwrap();
  ready(exchange.body_end(ClientGone::never())).unwrap();
  assert_eq!(exchange.reply_status().code, 207);
}

fn assert_real_git_get_serves(tools: &mut RealToolSites, target: &str, owner: &str) {
  let git_cmd = request("GET", target, &[owner]);
  let Answer::Serve { exchange, .. } = ready(tools.decide(&head(&git_cmd), false)).answer else {
    panic!("GET {target} must route to GitSite");
  };
  assert!(matches!(exchange, ToolExchange::Git(_)));
}

/// Gemini Suggestion 1: Do send `?cmd=capabilities` on SVN `GET`/`PROPFIND` and Git
/// `info/refs?service=git-upload-pack&cmd=capabilities`, plus real Hg `org/hgrepo?cmd=capabilities`.
/// Expect: SVN stays on `SvnSite`, Git stays on `GitSite`, and Hg still reaches `HgSite`.
#[test]
fn suggestion1_cmd_query_does_not_hijack_svn_or_git_while_hg_reaches_hg() {
  let mut tools = real_tool_sites_with_suffix_files();
  let owner = basic("alice", OWNER_TOKEN);

  assert_svn_get_returns_bytes(
    &mut tools,
    &owner,
    "/repo/trunk/HEAD?cmd=capabilities",
    b"svn-head\n",
  );
  assert_svn_propfind_207(&mut tools, "/repo?cmd=capabilities", &owner);
  assert_real_git_get_serves(
    &mut tools,
    "/r.git/info/refs?service=git-upload-pack&cmd=capabilities",
    &owner,
  );

  let mut nested = nested_tool_sites();
  let hg_multi = request("GET", "/org/hgrepo?cmd=capabilities", &[&owner]);
  assert!(matches!(
    ready(nested.decide(&head(&hg_multi), false)).answer,
    Answer::Refuse { status, .. } if status.code == 421
  ));
}

/// Counting authenticator to verify `ToolSites` proves credentials at most once per request.
struct CountingTokens<'a>(&'a std::cell::Cell<usize>);

impl Authenticator for CountingTokens<'_> {
  async fn prove(&self, credential: &Credential) -> Result<Proven, Unproven> {
    self.0.set(self.0.get().saturating_add(1));
    Tokens.prove(credential).await
  }
}

type CountingToolSites<'a> = ToolSites<
  GitSite<CountingTokens<'a>, Held, MockGitServices, MockLfs>,
  StubSite,
  SvnSite<CountingTokens<'a>, Held, Stores>,
>;

fn assert_auth_calls_on_refusal(
  tools: &mut CountingToolSites<'_>,
  calls: &std::cell::Cell<usize>,
  target: &str,
  headers: &[&str],
  expected: (u16, usize),
) {
  calls.set(0);
  let req = request("GET", target, headers);
  let decided = ready(tools.decide(&head(&req), false));
  let Answer::Refuse { status, .. } = decided.answer else {
    panic!("GET {target} must be refused");
  };
  assert_eq!((status.code, calls.get()), expected, "GET {target}");
}

/// Gemini Nit 2: Do send invalid-credential, missing-credential, and malformed-credential
/// requests through `ToolSites` backed by a counting authenticator. Expect: invalid credentials
/// invoke `prove()` exactly ONCE (never twice), and missing/malformed invoke `prove()` zero times.
#[test]
fn nit2_unauthenticated_tool_sites_authenticates_at_most_once() {
  let calls = std::cell::Cell::new(0usize);
  let repo: &'static Repo = Box::leak(Box::new(Repo::build()));
  let git = GitSite::new(
    REALM.to_owned(),
    ROOM,
    CountingTokens(&calls),
    Held,
    MockGitServices,
  )
  .with_lfs(MockLfs);
  let svn = SvnSite::new(
    REALM.to_owned(),
    ROOM,
    CountingTokens(&calls),
    Held,
    Stores { repo },
  );
  let mut tools = ToolSites::new(git, StubSite(421), svn, ROOM);

  for target in [
    "/repo/trunk/a.txt",
    "/r.git/info/refs?service=git-upload-pack",
  ] {
    assert_auth_calls_on_refusal(
      &mut tools,
      &calls,
      target,
      &["Authorization: Bearer wrong-token"],
      (401, 1),
    );
    assert_auth_calls_on_refusal(
      &mut tools,
      &calls,
      target,
      &["Authorization: Bearer a", "Authorization: Bearer b"],
      (400, 0),
    );
  }
}

/// Gemini Suggestion 2: Do call `status_of` for `405` and `503`. Expect: `405 Method Not Allowed`
/// and `503 Service Unavailable` are preserved instead of collapsing to `500`.
#[test]
fn suggestion2_status_of_preserves_405_and_503() {
  assert_eq!(super::status_of(405), Status::METHOD_NOT_ALLOWED);
  assert_eq!(super::status_of(503), Status::SERVICE_UNAVAILABLE);
}

/// Repository fixture granting `STRANGER` (`bob`) `PolicyRight::Read` only on `/`.
struct ReadOnlyHeld;

impl Repositories for ReadOnlyHeld {
  fn locate(&self, _name: &str) -> Located {
    Located::Here
  }

  fn find(&self, name: &str) -> Option<RepositoryRecord> {
    use slates_db::catalog::{
      PolicyEffect, PolicyEntry, PolicyNames, PolicyRight, PolicyRule, PolicyWho,
    };
    if name != "repo" {
      return None;
    }
    Some(RepositoryRecord {
      index_bits: 0,
      id: REPO,
      name: "repo".to_owned(),
      tool: VcsTool::Subversion,
      format: ObjectFormat::Revisions,
      default_branch: None,
      owner: principal(OWNER),
      policy: RepositoryPolicy {
        version: 1,
        rules: vec![PolicyRule {
          names: PolicyNames::Paths,
          pattern: "/".to_owned(),
          right: PolicyRight::Read,
          effect: PolicyEffect::Allow,
          who: PolicyWho::AnyOf {
            entries: vec![PolicyEntry::Is {
              principal: principal(STRANGER),
            }],
          },
        }],
        hooks: Vec::new(),
        hooks_env: Vec::new(),
        signers: Vec::new(),
        hg_publishing: true,
      },
    })
  }
}

fn assert_dav_reads_allowed(svn: &mut SvnSite<Tokens, ReadOnlyHeld, Stores>, reader: &str) {
  for (method, target) in [
    ("GET", "/repo/trunk/a.txt"),
    ("HEAD", "/repo/trunk/a.txt"),
    ("OPTIONS", "/repo"),
    ("PROPFIND", "/repo"),
    ("REPORT", "/repo/!svn/me"),
  ] {
    let req = request(method, target, &[reader]);
    assert!(
      matches!(ready(svn.answer(&head(&req))).answer, Answer::Serve { .. }),
      "read-only principal must be allowed {method} {target}"
    );
  }
}

fn assert_dav_writes_gated(
  svn: &mut SvnSite<Tokens, ReadOnlyHeld, Stores>,
  reader: &str,
  owner: &str,
) {
  for (method, target) in [
    ("POST", "/repo/!svn/me"),
    ("MKACTIVITY", "/repo/!svn/act/act-1"),
    ("CHECKOUT", "/repo/!svn/vcc/default"),
    ("LOCK", "/repo/trunk/a.txt"),
    ("UNLOCK", "/repo/trunk/a.txt"),
    ("PROPPATCH", "/repo/!svn/rev/1"),
    ("PUT", "/repo/!svn/txr/1-1/trunk/a.txt"),
    ("DELETE", "/repo/!svn/txr/1-1/trunk/a.txt"),
    ("MKCOL", "/repo/!svn/txr/1-1/trunk/newdir"),
    ("COPY", "/repo/!svn/rvr/1/trunk/a.txt"),
    ("MOVE", "/repo/trunk/a.txt"),
    ("MERGE", "/repo"),
  ] {
    let req = request(method, target, &[reader]);
    let Answer::Refuse { status, .. } = ready(svn.answer(&head(&req))).answer else {
      panic!("read-only principal must be refused 403 on {method} {target}");
    };
    assert_eq!(status, Status::FORBIDDEN, "{method} {target}");
    let owner_req = request(method, target, &[owner]);
    assert!(
      matches!(
        ready(svn.answer(&head(&owner_req))).answer,
        Answer::Serve { .. }
      ),
      "owner must be allowed {method} {target}"
    );
  }
}

/// Gemini Suggestion 3 (`http://` WebDAV): Do send read-side and write-side DAV methods as a
/// principal granted only `PolicyRight::Read` (and compare against the owner). Expect: read-side
/// methods (`GET`, `HEAD`, `OPTIONS`, `PROPFIND`, `REPORT`) succeed (`Serve`), while every
/// write-side DAV method (`POST !svn/me`, `MKACTIVITY`, `CHECKOUT`, `LOCK`, `UNLOCK`, `PROPPATCH`,
/// `PUT`, `DELETE`, `MKCOL`, `COPY`, `MOVE`, `MERGE`) is refused `403 Forbidden` before opening
/// any transaction or exchange.
#[test]
fn suggestion3_read_only_principal_allowed_dav_reads_and_refused_403_on_dav_writes() {
  let repo: &'static Repo = Box::leak(Box::new(Repo::build()));
  let mut svn = SvnSite::new(
    REALM.to_owned(),
    ROOM,
    Tokens,
    ReadOnlyHeld,
    Stores { repo },
  );
  let reader = basic("bob", STRANGER_TOKEN);
  let owner = basic("alice", OWNER_TOKEN);
  assert_dav_reads_allowed(&mut svn, &reader);
  assert_dav_writes_gated(&mut svn, &reader, &owner);
}

/// Shape: upper bound on cross-shard hops through [`ToolSites`] before failing for ping-pong.
const MAX_TOOL_SITES_HOPS: usize = 4;

/// Two-shard catalog view for testing cross-shard [`ToolSites`] routing:
/// - Shard `0` owns `"alpha.git"` (absent), `"team/app.git"` (absent), `"ghost.git"` (absent),
///   and SVN `"team"` (present).
/// - Shard `1` owns Git `"alpha"` (present), Git `"team/app"` (present), and `"ghost"` (absent).
struct TwoShardToolCatalog {
  shard: u16,
}

impl TwoShardToolCatalog {
  fn owner_of(name: &str) -> u16 {
    if name.ends_with(".git") || name == "team" {
      0
    } else {
      1
    }
  }
}

impl Repositories for TwoShardToolCatalog {
  fn locate(&self, name: &str) -> Located {
    let owner = Self::owner_of(name);
    if owner == self.shard {
      Located::Here
    } else {
      Located::Elsewhere(owner)
    }
  }

  fn find(&self, name: &str) -> Option<RepositoryRecord> {
    let (id, tool, format) = match (self.shard, name) {
      (0, "team") => (REPO, VcsTool::Subversion, ObjectFormat::Revisions),
      (1, "alpha") => (
        RepositoryId { bytes: [21; 16] },
        VcsTool::Git,
        ObjectFormat::Sha1,
      ),
      (1, "team/app") => (
        RepositoryId { bytes: [22; 16] },
        VcsTool::Git,
        ObjectFormat::Sha1,
      ),
      _ => return None,
    };
    Some(RepositoryRecord {
      index_bits: 0,
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
        hg_publishing: true,
      },
    })
  }
}

type TwoShardTools = ToolSites<
  GitSite<Tokens, TwoShardToolCatalog, MockGitServices, MockLfs>,
  StubSite,
  SvnSite<Tokens, TwoShardToolCatalog, Stores>,
>;

/// Drives `req` across simulated shards `0` and `1` through full [`ToolSites`] carrying
/// `route_stage()` across every `Answer::Move`.
fn run_two_shard_tool_sites(
  start_shard: u16,
  req: &[u8],
  repo: &'static Repo,
) -> (usize, Decided<<TwoShardTools as Site>::Exchange>) {
  let mut shard = start_shard;
  let mut stage = crate::vcs_http::RouteStage::Initial;
  for hops in 0..=MAX_TOOL_SITES_HOPS {
    let git = GitSite::new(
      REALM.to_owned(),
      ROOM,
      Tokens,
      TwoShardToolCatalog { shard },
      MockGitServices,
    )
    .with_lfs(MockLfs);
    let svn = SvnSite::new(
      REALM.to_owned(),
      ROOM,
      Tokens,
      TwoShardToolCatalog { shard },
      Stores { repo },
    );
    let mut tools = ToolSites::new(git, StubSite(421), svn, ROOM);
    tools.set_route_stage(stage);
    let decided = ready(tools.decide(&head(req), false));
    match decided.answer {
      Answer::Move { shard: next } => {
        stage = tools.route_stage();
        shard = next;
      }
      _ => return (hops, decided),
    }
  }
  panic!(
    "ToolSites cross-shard routing ping-ponged past {MAX_TOOL_SITES_HOPS} hops from shard {start_shard}"
  );
}

fn assert_two_shard_git_alias_serves(req: &[u8], repo: &'static Repo, label: &str) {
  for start in [0_u16, 1_u16] {
    let (hops, decided) = run_two_shard_tool_sites(start, req, repo);
    assert!(hops <= 2, "{label} from shard {start} took {hops} hops");
    assert!(matches!(
      decided.answer,
      Answer::Serve {
        exchange: ToolExchange::Git(_),
        ..
      }
    ));
  }
}

/// Blocker 1 + Blocker 2 end-to-end across two shards in [`ToolSites`]:
/// Do send single-segment LFS alias `POST /alpha.git/info/lfs/objects/batch`, multi-segment LFS
/// alias `PUT /team/app.git/info/lfs/objects/<64hex>` while SVN `"team"` exists on Shard 0, and
/// absent `POST /ghost.git/info/lfs/objects/batch` from both Shard 0 and Shard 1.
/// Expect: every request terminates in `<= 2` moves with zero ping-pong, `"alpha.git"` and
/// `"team/app.git"` serve via `ToolExchange::Git`, and `"ghost.git"` refuses `404`.
#[test]
fn blocker2_tool_sites_cross_shard_lfs_dot_git_alias_terminates_without_ping_pong() {
  let repo: &'static Repo = Box::leak(Box::new(repo_with_spoofed_git_paths()));
  let owner = basic("alice", OWNER_TOKEN);
  let lfs_headers = [
    owner.as_str(),
    "Accept: application/vnd.git-lfs+json",
    "Content-Type: application/vnd.git-lfs+json",
  ];
  let alpha_req = request("POST", "/alpha.git/info/lfs/objects/batch", &lfs_headers);
  assert_two_shard_git_alias_serves(&alpha_req, repo, "alpha.git");

  let nested_put_target = format!("/team/app.git/info/lfs/objects/{LFS_OID_HEX}");
  let nested_req = request("PUT", &nested_put_target, &[&owner]);
  assert_two_shard_git_alias_serves(&nested_req, repo, "team/app.git");

  let ghost_req = request("POST", "/ghost.git/info/lfs/objects/batch", &lfs_headers);
  let (hops, decided) = run_two_shard_tool_sites(1, &ghost_req, repo);
  assert!(hops <= 2, "ghost.git took {hops} hops");
  let Answer::Refuse { status, .. } = decided.answer else {
    panic!("ghost.git must refuse 404");
  };
  assert_eq!(status, Status::NOT_FOUND);
}

fn assert_unauthenticated_401_with_challenge(
  tools: &mut RealToolSites,
  method: &str,
  target: &str,
) {
  let req = request(method, target, &[]);
  let decided = ready(tools.decide(&head(&req), false));
  assert!(!decided.proven, "{method} {target}");
  let Answer::Refuse { status, fields } = decided.answer else {
    panic!("{method} {target} must refuse 401");
  };
  assert_eq!(status.code, 401, "{method} {target}");
  assert!(
    fields
      .iter()
      .any(|(name, _)| name.eq_ignore_ascii_case(b"WWW-Authenticate")),
    "{method} {target} must carry WWW-Authenticate"
  );
}

/// Auth-before-existence parity with `?cmd=capabilities`: Do send unauthenticated `GET`,
/// `PROPFIND`, and Git `info/refs?service=git-upload-pack&cmd=capabilities` across nonexistent,
/// SVN, Git, and Hg repositories. Expect: identical `401 Unauthorized` with `WWW-Authenticate`
/// across all four repository states, leaking neither existence nor tool.
#[test]
fn suggestion1_unauthenticated_cmd_query_preserves_401_parity_across_all_repo_states() {
  let mut tools = real_tool_sites_with_suffix_files();
  let probes = [
    ("GET", "/trunk/hello.txt?cmd=capabilities"),
    ("PROPFIND", "?cmd=capabilities"),
    ("GET", "/info/refs?service=git-upload-pack&cmd=capabilities"),
  ];
  for (method, suffix) in probes {
    for repo_name in ["no-such", "repo", "r.git", "hg"] {
      let target = format!("/{repo_name}{suffix}");
      assert_unauthenticated_401_with_challenge(&mut tools, method, &target);
    }
  }
}

/// Shape: SVN-planted spoof files matching every Git/LFS suffix tested in Blocker 1.
const HG_SHADOW_SPOOF_FILES: &[(&str, &[u8])] = &[
  ("/HEAD", b"SPOOFED_SVN_HEAD\n"),
  ("/info/refs", b"SPOOFED_SVN_REFS\n"),
  ("/info/lfs/locks", b"SPOOFED_SVN_LFS_LOCKS\n"),
  ("/team/HEAD", b"SPOOFED_SVN_ORG_TEAM_HEAD\n"),
];

fn repo_with_hg_shadow_spoof_files() -> Repo {
  let dirs = ["/info", "/info/lfs", "/team"];
  let mut changes: Vec<ChangedPath> = dirs
    .iter()
    .map(|path| added(path, NodeKind::Directory, None, None))
    .collect();
  let mut tree_r1 = BTreeMap::from([("/".to_owned(), true)]);
  for dir in dirs {
    tree_r1.insert(dir.to_owned(), true);
  }
  let mut texts = BTreeMap::new();
  for (path, bytes) in HG_SHADOW_SPOOF_FILES {
    let text = text_of(bytes);
    texts.insert(text.identity, bytes.to_vec());
    changes.push(added(path, NodeKind::File, Some(text), None));
    tree_r1.insert((*path).to_owned(), false);
  }
  let mut index = Index::new();
  index
    .append(
      RevisionRecord {
        transaction: "0-1".to_owned(),
        changes,
      },
      usize::try_from(ROOM).unwrap(),
    )
    .unwrap();
  let date = |value: &str| (b"svn:date".to_vec(), value.as_bytes().to_vec());
  Repo {
    index,
    trees: vec![BTreeMap::from([("/".to_owned(), true)]), tree_r1],
    lists: BTreeMap::new(),
    texts,
    revisions: vec![
      vec![date("2026-11-18T00:00:00.000000Z")],
      vec![
        (b"svn:author".to_vec(), b"alice".to_vec()),
        date(DATE),
        (b"svn:log".to_vec(), b"spoof hg git suffixes".to_vec()),
      ],
    ],
    locks: LockTable::new(),
    authz: Authz::parse(AUTHZ, None, authz::Bounds::of(AUTHZ, None)).unwrap(),
  }
}

/// Multi-shard catalog spanning 8 distinct shards to test multi-segment Mercurial repos ending in
/// Git/LFS suffixes, Git/SVN coexistence, and strictly monotonic cross-shard hops.
struct EightShardHgShadowCatalog {
  shard: u16,
}

impl EightShardHgShadowCatalog {
  fn owner_of(name: &str) -> u16 {
    match name {
      "team/HEAD" | "team/info/refs" | "team/info/lfs/locks" | "team/git-upload-pack" => 0,
      "org/team/HEAD" | "team/missing.git/info/lfs/objects/batch" => 1,
      "gprefix/HEAD" | "team/missing.git" => 2,
      "ns/app/HEAD" | "team/missing" => 3,
      "team" | "org" | "plain" => 4,
      "gprefix" | "team/app" => 5,
      "ns/app" => 6,
      _ => 7,
    }
  }
}

impl Repositories for EightShardHgShadowCatalog {
  fn locate(&self, name: &str) -> Located {
    let owner = Self::owner_of(name);
    if owner == self.shard {
      Located::Here
    } else {
      Located::Elsewhere(owner)
    }
  }

  fn find(&self, name: &str) -> Option<RepositoryRecord> {
    if Self::owner_of(name) != self.shard {
      return None;
    }
    let (id_byte, tool, format) = match name {
      "team" | "org" | "plain" => (0, VcsTool::Subversion, ObjectFormat::Revisions),
      "gprefix" | "ns/app" | "team/app" => (31, VcsTool::Git, ObjectFormat::Sha1),
      "team/HEAD"
      | "team/info/refs"
      | "team/info/lfs/locks"
      | "team/git-upload-pack"
      | "org/team/HEAD"
      | "gprefix/HEAD"
      | "ns/app/HEAD" => (32, VcsTool::Mercurial, ObjectFormat::Sha1),
      _ => return None,
    };
    let id = if id_byte == 0 {
      REPO
    } else {
      RepositoryId {
        bytes: [id_byte; 16],
      }
    };
    Some(RepositoryRecord {
      index_bits: 0,
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
        hg_publishing: true,
      },
    })
  }
}

/// Shape: upper bound on cross-shard hops across the 6 monotonic `RouteStage` levels.
const MAX_EIGHT_SHARD_HOPS: usize = 6;

/// Minimal Mercurial site stub backed by `EightShardHgShadowCatalog` that mirrors real
/// `HgSite`'s cross-shard `locate`/`find` (`Move` -> `421` marker for Hg -> `404` if absent).
struct CatalogHgStub(EightShardHgShadowCatalog);

impl Site for CatalogHgStub {
  type Exchange = StubExchange;

  async fn decide(&mut self, head: &Head<'_>, _secure: bool) -> Decided<Self::Exchange> {
    let Some(name) = crate::vcs_http::hg::wire_repo_candidate(head, ROOM) else {
      return Decided {
        answer: Answer::Refuse {
          status: Status::NOT_FOUND,
          fields: Vec::new(),
        },
        proven: true,
      };
    };
    if let Located::Elsewhere(shard) = self.0.locate(&name) {
      return Decided {
        answer: Answer::Move { shard },
        proven: true,
      };
    }
    let status = if self
      .0
      .find(&name)
      .is_some_and(|record| record.tool == VcsTool::Mercurial)
    {
      Status {
        code: 421,
        reason: "HgMatched",
      }
    } else {
      Status::NOT_FOUND
    };
    Decided {
      answer: Answer::Refuse {
        status,
        fields: Vec::new(),
      },
      proven: true,
    }
  }
}

type EightShardTools = ToolSites<
  GitSite<Tokens, EightShardHgShadowCatalog, MockGitServices, MockLfs>,
  CatalogHgStub,
  SvnSite<Tokens, EightShardHgShadowCatalog, Stores>,
>;

fn run_eight_shard_tool_sites(
  start_shard: u16,
  req: &[u8],
  repo: &'static Repo,
) -> (usize, Decided<<EightShardTools as Site>::Exchange>) {
  let mut shard = start_shard;
  let mut stage = crate::vcs_http::RouteStage::Initial;
  for hops in 0..=MAX_EIGHT_SHARD_HOPS {
    let git = GitSite::new(
      REALM.to_owned(),
      ROOM,
      Tokens,
      EightShardHgShadowCatalog { shard },
      MockGitServices,
    )
    .with_lfs(MockLfs);
    let svn = SvnSite::new(
      REALM.to_owned(),
      ROOM,
      Tokens,
      EightShardHgShadowCatalog { shard },
      Stores { repo },
    );
    let mut tools = ToolSites::new(
      git,
      CatalogHgStub(EightShardHgShadowCatalog { shard }),
      svn,
      ROOM,
    );
    tools.set_route_stage(stage);
    let decided = ready(tools.decide(&head(req), false));
    match decided.answer {
      Answer::Move { shard: next } => {
        stage = tools.route_stage();
        shard = next;
      }
      _ => {
        assert_eq!(
          tools.route_stage(),
          crate::vcs_http::RouteStage::Initial,
          "terminal decision must reset route_stage to Initial"
        );
        return (hops, decided);
      }
    }
  }
  panic!(
    "ToolSites cross-shard routing exceeded {MAX_EIGHT_SHARD_HOPS} hops from shard {start_shard}"
  );
}

fn assert_eight_shard_hg_suffixes(repo: &'static Repo, owner: &str) {
  for (method, target) in [
    ("GET", "/team/HEAD?cmd=capabilities"),
    ("GET", "/team/info/refs?cmd=capabilities"),
    ("GET", "/team/info/lfs/locks?cmd=capabilities"),
    ("GET", "/team/git-upload-pack?cmd=capabilities"),
    ("POST", "/team/git-upload-pack?cmd=unbundle"),
    ("GET", "/org/team/HEAD?cmd=capabilities"),
    ("GET", "/gprefix/HEAD?cmd=capabilities"),
    ("GET", "/ns/app/HEAD?cmd=capabilities"),
  ] {
    let req = request(method, target, &[owner]);
    let (_hops, decided) = run_eight_shard_tool_sites(7, &req, repo);
    assert!(
      matches!(decided.answer, Answer::Refuse { status, .. } if status.code == 421),
      "{method} {target} must route to HgSite (421), never shadowed by SVN/Git"
    );
  }
}

fn assert_eight_shard_coexistence_and_missing(repo: &'static Repo, owner: &str) {
  let git_with_cmd = request(
    "GET",
    "/team/app/info/refs?service=git-upload-pack&cmd=capabilities",
    &[owner],
  );
  let (_hops, decided) = run_eight_shard_tool_sites(7, &git_with_cmd, repo);
  assert!(
    matches!(
      decided.answer,
      Answer::Serve {
        exchange: ToolExchange::Git(_),
        ..
      }
    ),
    "Git team/app must still serve when no Hg team/app/info/refs exists"
  );

  let git_dumb = request("GET", "/ns/app/HEAD", &[owner]);
  let (_hops, decided) = run_eight_shard_tool_sites(7, &git_dumb, repo);
  assert!(
    matches!(decided.answer, Answer::Refuse { status, .. } if status.code == 403),
    "non-cmd GET /ns/app/HEAD must stay on GitSite (403 DumbProtocol)"
  );

  let svn_fallback = request("GET", "/plain/HEAD?cmd=capabilities", &[owner]);
  let (_hops, decided) = run_eight_shard_tool_sites(7, &svn_fallback, repo);
  assert!(
    matches!(
      decided.answer,
      Answer::Serve {
        exchange: ToolExchange::Svn(_),
        ..
      }
    ),
    "SVN plain/HEAD?cmd=capabilities must serve from SvnSite when no Hg plain/HEAD exists"
  );
}

fn assert_eight_shard_missing_lfs_bounded(repo: &'static Repo, owner: &str) {
  let lfs_headers = [
    owner,
    "Accept: application/vnd.git-lfs+json",
    "Content-Type: application/vnd.git-lfs+json",
  ];
  for target in [
    "/team/missing.git/info/lfs/objects/batch",
    "/team/missing.git/info/lfs/objects/batch?cmd=capabilities",
  ] {
    let req = request("POST", target, &lfs_headers);
    let (hops, decided) = run_eight_shard_tool_sites(7, &req, repo);
    assert!(hops <= MAX_EIGHT_SHARD_HOPS, "{target} took {hops} hops");
    assert!(
      matches!(decided.answer, Answer::Refuse { status, .. } if status == Status::NOT_FOUND),
      "{target} must terminate 404 without livelock"
    );
  }
}

/// Round-4 Blocker 1: Do route 2/3/4-segment Mercurial repos ending in Git/LFS suffixes
/// (`team/HEAD`, `team/info/refs`, `team/info/lfs/locks`, `team/git-upload-pack`, `org/team/HEAD`,
/// `gprefix/HEAD`, `ns/app/HEAD`), Git smart/dumb coexistences, SVN fallback (`plain/HEAD`), and
/// absent `team/missing.git/info/lfs/objects/batch` across 8 distinct shards. Expect: every
/// registered Hg repo dispatches to `HgSite` (`421` stub), Git smart/dumb routes stay on `GitSite`,
/// SVN fallback serves SVN `/HEAD`, and missing LFS paths terminate `404` within bounded hops.
#[test]
fn round4_blocker1_multi_segment_hg_git_suffixes_and_eight_shard_monotonicity() {
  let repo: &'static Repo = Box::leak(Box::new(repo_with_hg_shadow_spoof_files()));
  let owner = basic("alice", OWNER_TOKEN);
  assert_eight_shard_hg_suffixes(repo, &owner);
  assert_eight_shard_coexistence_and_missing(repo, &owner);
  assert_eight_shard_missing_lfs_bounded(repo, &owner);
}

/// Round-4 Suggestion 1: Do send `POST /repo/%21svn/me` (percent-encoded `!svn`) through
/// `ToolSites` as the owner and unauthenticated. Expect: owner dispatches to `ToolExchange::Svn`
/// (identical to `POST /repo/!svn/me`, never `404` via `GitSite`) and unauthenticated gets `401`.
#[test]
fn round4_suggestion1_percent_encoded_special_segment_routes_to_svn() {
  let mut tools = real_tool_sites_with_suffix_files();
  let owner = basic("alice", OWNER_TOKEN);
  let req = request(
    "POST",
    "/repo/%21svn/me",
    &[&owner, "Content-Type: application/vnd.svn-skel"],
  );
  assert!(
    matches!(
      ready(tools.decide(&head(&req), false)).answer,
      Answer::Serve {
        exchange: ToolExchange::Svn(_),
        ..
      }
    ),
    "POST /repo/%21svn/me must route to SvnSite"
  );
  assert_unauthenticated_401_with_challenge(&mut tools, "POST", "/repo/%21svn/me");
}

/// Configurable policy fixture for HTTP WebDAV `PolicyEffect::Deny` and per-path authz tests.
struct PolicyHeld {
  rules: Vec<slates_db::catalog::PolicyRule>,
}

impl Repositories for PolicyHeld {
  fn locate(&self, _name: &str) -> Located {
    Located::Here
  }

  fn find(&self, name: &str) -> Option<RepositoryRecord> {
    if name != "repo" {
      return None;
    }
    Some(RepositoryRecord {
      index_bits: 0,
      id: REPO,
      name: "repo".to_owned(),
      tool: VcsTool::Subversion,
      format: ObjectFormat::Revisions,
      default_branch: None,
      owner: principal(OWNER),
      policy: RepositoryPolicy {
        version: 1,
        rules: self.rules.clone(),
        hooks: Vec::new(),
        hooks_env: Vec::new(),
        signers: Vec::new(),
        hg_publishing: true,
      },
    })
  }
}

fn svn_site_with_rules(
  repo: &'static Repo,
  rules: Vec<slates_db::catalog::PolicyRule>,
) -> SvnSite<Tokens, PolicyHeld, Stores> {
  SvnSite::new(
    REALM.to_owned(),
    ROOM,
    Tokens,
    PolicyHeld { rules },
    Stores { repo },
  )
}

fn assert_dav_status(
  svn: &mut SvnSite<Tokens, PolicyHeld, Stores>,
  auth: &str,
  method: &str,
  target: &str,
  expected_serve: bool,
) {
  let req = request(method, target, &[auth]);
  let answer = ready(svn.answer(&head(&req))).answer;
  if expected_serve {
    assert!(
      matches!(answer, Answer::Serve { .. }),
      "{method} {target} must be allowed"
    );
  } else {
    let Answer::Refuse { status, .. } = answer else {
      panic!("{method} {target} must be refused 403");
    };
    assert_eq!(status, Status::FORBIDDEN, "{method} {target}");
  }
}

/// Round-4 Blocker 2 + Suggestion 2 (HTTP WebDAV `PolicyEffect::Deny`, per-path `Deny`,
/// subpath `Allow` under root `Deny`, `Submit`-only, and owner `Deny Submit`): Do drive `SvnSite`
/// across all six policy shapes. Expect: `Deny Read /` and `Submit`-only refuse all methods `403`;
/// `Deny Submit /` (non-owner and owner) permits reads and refuses all writes `403`; per-path
/// `Deny Read /secret` refuses `GET`/`PROPFIND`/`!svn/rvr` under `/secret` while allowing `/trunk`;
/// subpath `Allow Read /trunk` under root `Deny Read /` allows `OPTIONS /repo` and `/trunk` reads
/// while refusing `/secret`.
#[test]
fn round4_blocker2_http_webdav_enforces_deny_rules_per_path_and_submit_only() {
  use slates_db::catalog::{
    PolicyEffect, PolicyEntry, PolicyNames, PolicyRight, PolicyRule, PolicyWho,
  };
  let repo: &'static Repo = Box::leak(Box::new(Repo::build()));
  let stranger = basic("bob", STRANGER_TOKEN);
  let owner = basic("alice", OWNER_TOKEN);
  let only = |uid: u32| PolicyWho::AnyOf {
    entries: vec![PolicyEntry::Is {
      principal: principal(uid),
    }],
  };
  let rule = |pattern: &str, right: PolicyRight, effect: PolicyEffect, who: PolicyWho| PolicyRule {
    names: PolicyNames::Paths,
    pattern: pattern.to_owned(),
    right,
    effect,
    who,
  };

  // 1. [Allow Read / Anyone, Deny Read / STRANGER] -> 403 on all reads & writes.
  let mut deny_read = svn_site_with_rules(
    repo,
    vec![
      rule(
        "/",
        PolicyRight::Read,
        PolicyEffect::Allow,
        PolicyWho::Anyone,
      ),
      rule("/", PolicyRight::Read, PolicyEffect::Deny, only(STRANGER)),
    ],
  );
  for (m, t) in [
    ("GET", "/repo/trunk/a.txt"),
    ("OPTIONS", "/repo"),
    ("PROPFIND", "/repo"),
    ("REPORT", "/repo/!svn/me"),
    ("POST", "/repo/!svn/me"),
  ] {
    assert_dav_status(&mut deny_read, &stranger, m, t, false);
  }

  // 2. [Allow Read / Anyone, Allow Submit / Anyone, Deny Submit / STRANGER] -> reads ok, writes 403.
  let mut deny_write = svn_site_with_rules(
    repo,
    vec![
      rule(
        "/",
        PolicyRight::Read,
        PolicyEffect::Allow,
        PolicyWho::Anyone,
      ),
      rule(
        "/",
        PolicyRight::Submit,
        PolicyEffect::Allow,
        PolicyWho::Anyone,
      ),
      rule("/", PolicyRight::Submit, PolicyEffect::Deny, only(STRANGER)),
    ],
  );
  assert_dav_status(&mut deny_write, &stranger, "GET", "/repo/trunk/a.txt", true);
  for (m, t) in [
    ("POST", "/repo/!svn/me"),
    ("MKACTIVITY", "/repo/!svn/act/a1"),
    ("LOCK", "/repo/trunk/a.txt"),
    ("UNLOCK", "/repo/trunk/a.txt"),
    ("PROPPATCH", "/repo/!svn/rev/1"),
    ("MERGE", "/repo"),
  ] {
    assert_dav_status(&mut deny_write, &stranger, m, t, false);
  }

  // 3. Submit-only [Allow Submit / STRANGER] (no Read) -> 403 on both reads & writes.
  let mut submit_only = svn_site_with_rules(
    repo,
    vec![rule(
      "/",
      PolicyRight::Submit,
      PolicyEffect::Allow,
      only(STRANGER),
    )],
  );
  assert_dav_status(
    &mut submit_only,
    &stranger,
    "GET",
    "/repo/trunk/a.txt",
    false,
  );
  assert_dav_status(&mut submit_only, &stranger, "POST", "/repo/!svn/me", false);

  // 4. Owner with explicit Deny Submit / owner -> reads ok, writes 403.
  let mut owner_ro = svn_site_with_rules(
    repo,
    vec![rule(
      "/",
      PolicyRight::Submit,
      PolicyEffect::Deny,
      only(OWNER),
    )],
  );
  assert_dav_status(&mut owner_ro, &owner, "GET", "/repo/trunk/a.txt", true);
  assert_dav_status(&mut owner_ro, &owner, "POST", "/repo/!svn/me", false);

  // 5. Per-path Deny Read /secret -> /trunk ok, /secret + !svn/rvr/1/secret 403.
  let mut path_deny = svn_site_with_rules(
    repo,
    vec![
      rule(
        "/",
        PolicyRight::Read,
        PolicyEffect::Allow,
        PolicyWho::Anyone,
      ),
      rule(
        "/secret",
        PolicyRight::Read,
        PolicyEffect::Deny,
        PolicyWho::Anyone,
      ),
    ],
  );
  assert_dav_status(&mut path_deny, &stranger, "GET", "/repo/trunk/a.txt", true);
  assert_dav_status(
    &mut path_deny,
    &stranger,
    "GET",
    "/repo/secret/b.txt",
    false,
  );
  assert_dav_status(
    &mut path_deny,
    &stranger,
    "GET",
    "/repo/!svn/rvr/1/secret/b.txt",
    false,
  );
  assert_dav_status(&mut path_deny, &stranger, "PROPFIND", "/repo/secret", false);

  // 6. Subpath Allow /trunk under root Deny / -> OPTIONS + /trunk ok, /secret 403.
  let mut sub_allow = svn_site_with_rules(
    repo,
    vec![
      rule(
        "/",
        PolicyRight::Read,
        PolicyEffect::Allow,
        PolicyWho::Anyone,
      ),
      rule(
        "/",
        PolicyRight::Read,
        PolicyEffect::Deny,
        PolicyWho::Anyone,
      ),
      rule(
        "/trunk",
        PolicyRight::Read,
        PolicyEffect::Allow,
        only(STRANGER),
      ),
    ],
  );
  assert_dav_status(&mut sub_allow, &stranger, "OPTIONS", "/repo", true);
  assert_dav_status(&mut sub_allow, &stranger, "GET", "/repo/trunk/a.txt", true);
  assert_dav_status(
    &mut sub_allow,
    &stranger,
    "GET",
    "/repo/!svn/rvr/1/trunk/a.txt",
    true,
  );
  assert_dav_status(
    &mut sub_allow,
    &stranger,
    "GET",
    "/repo/secret/b.txt",
    false,
  );
}

/// Revision 1 for the Blocker-2 DAV per-path read-rule test: `/pub/ok.txt`, `/pub/secret/vault.txt`,
/// `/secret/top.txt`, and `/trunk/ok.txt`.
fn dav_deny_revision() -> slates_svn::commit::revision::Revision {
  let ok = b"public-ok\n";
  let vault = b"TOP-SECRET-VAULT\n";
  let root_secret = b"TOP-SECRET-ROOT\n";
  let trunk_ok = b"trunk-ok\n";
  slates_svn::commit::revision::Revision {
    head: 0,
    record: RevisionRecord {
      transaction: "0-1".to_owned(),
      changes: vec![
        added_row("/pub", NodeKind::Directory, None),
        added_row("/pub/ok.txt", NodeKind::File, Some(ok)),
        added_row("/pub/secret", NodeKind::Directory, None),
        added_row("/pub/secret/vault.txt", NodeKind::File, Some(vault)),
        added_row("/secret", NodeKind::Directory, None),
        added_row("/secret/top.txt", NodeKind::File, Some(root_secret)),
        added_row("/trunk", NodeKind::Directory, None),
        added_row("/trunk/ok.txt", NodeKind::File, Some(trunk_ok)),
      ],
    },
    properties: BTreeMap::from([
      (b"svn:author".to_vec(), b"alice".to_vec()),
      (b"svn:date".to_vec(), DATE.as_bytes().to_vec()),
      (
        b"svn:log".to_vec(),
        b"seed secret and public trees".to_vec(),
      ),
    ]),
    contents: [
      (blake(ok), ok.to_vec()),
      (blake(vault), vault.to_vec()),
      (blake(root_secret), root_secret.to_vec()),
      (blake(trunk_ok), trunk_ok.to_vec()),
    ]
    .into_iter()
    .collect(),
    stored: BTreeMap::new(),
    lock_entries: Vec::new(),
  }
}

/// Runs one HTTP request `(method, target, extra_headers, body)` against the live shard's
/// `SvnSite` (`ShardRepositories` + `ServedRecords`) and returns `(status_code, body_utf8)`.
async fn run_live_dav(
  method: &str,
  target: &str,
  extra_headers: &[&str],
  body: &[u8],
) -> (u16, String) {
  let mut site = SvnSite::new(
    REALM.to_owned(),
    ROOM,
    Tokens,
    super::super::daemon_edge::ShardRepositories,
    ServedRecords,
  );
  let raw = request(method, target, extra_headers);
  let decided = site.answer(&head(&raw)).await;
  match decided.answer {
    Answer::Refuse { status, .. } => (status.code, String::new()),
    Answer::Serve { mut exchange, .. } => {
      if !body.is_empty() {
        exchange.body(body).await.unwrap();
      }
      exchange.body_end(ClientGone::never()).await.unwrap();
      let status = exchange.reply_status().code;
      let mut out = Vec::new();
      while exchange.reply(&mut out, ClientGone::never()).await.unwrap() {}
      (status, String::from_utf8_lossy(&out).into_owned())
    }
    Answer::Move { shard } => panic!("unexpected shard move to {shard}"),
  }
}

fn ipc_path_rule(
  pattern: &str,
  right: slates_ipc::protocol::PolicyRight,
  effect: slates_ipc::protocol::PolicyEffect,
  who: slates_ipc::protocol::PolicyWho,
) -> slates_ipc::protocol::PolicyRule {
  slates_ipc::protocol::PolicyRule {
    names: slates_ipc::protocol::PolicyNames::Paths,
    pattern: pattern.to_owned(),
    right,
    effect,
    who,
  }
}

fn set_repo_policy_on_first(
  daemon: &crate::Daemon,
  name: &str,
  read: u64,
  rules: Vec<slates_ipc::protocol::PolicyRule>,
) {
  let owner = daemon_owner();
  let repo = name.to_owned();
  let reply = crate::daemon::observe_first(daemon, move |state| {
    crate::verbs::dispatch(
      state,
      1,
      &owner,
      slates_ipc::protocol::RequestBody::SetRepositoryPolicy {
        repository: repo,
        read,
        rules,
        hooks: Vec::new(),
        hooks_env: Vec::new(),
        signers: Vec::new(),
        hg_publishing: true,
      },
    )
  });
  assert_eq!(reply, slates_ipc::protocol::ReplyBody::Acknowledged);
}

fn dav_update_report_body(src_path: &str) -> String {
  format!(
    "<S:update-report send-all=\"true\" xmlns:S=\"svn:\">\
     <S:src-path>{src_path}</S:src-path>\
     <S:target-revision>1</S:target-revision>\
     <S:entry rev=\"0\" start-empty=\"true\"></S:entry>\
     </S:update-report>"
  )
}

fn assert_dav_checkout_hides_denied(res: &(u16, String)) {
  assert_eq!(res.0, 200, "REPORT !svn/me checkout: {}", res.1);
  assert!(res.1.contains("ok.txt"), "public file missing: {}", res.1);
  assert!(
    !res.1.contains("TOP-SECRET"),
    "REPORT !svn/me leaked secret bytes: {}",
    res.1
  );
  assert!(
    !res.1.contains("vault.txt"),
    "REPORT !svn/me leaked denied paths: {}",
    res.1
  );
}

fn assert_dav_vcc_denied_root(res: &(u16, String)) {
  assert_eq!(
    res.0, 403,
    "REPORT !svn/vcc/default on /secret must be 403: {}",
    res.1
  );
  assert!(
    res.1.contains("220000"),
    "REPORT !svn/vcc/default must carry SVN_ERR_AUTHZ_ROOT_UNREADABLE: {}",
    res.1
  );
  assert!(
    !res.1.contains("TOP-SECRET"),
    "vcc leaked secret: {}",
    res.1
  );
}

fn assert_dav_propfind_hides_secret(res: &(u16, String), expected: &str, label: &str) {
  assert_eq!(res.0, 207, "{label} status: {}", res.1);
  assert!(res.1.contains(expected), "{label} missing entry: {}", res.1);
  assert!(!res.1.contains("secret"), "{label} leaked: {}", res.1);
}

fn assert_dav_log_hides_denied(res: &(u16, String)) {
  assert_eq!(res.0, 200, "REPORT log-report status: {}", res.1);
  assert!(
    res.1.contains("/pub/ok.txt"),
    "REPORT log-report missing allowed path: {}",
    res.1
  );
  assert!(
    !res.1.contains("/secret"),
    "REPORT log-report leaked /secret: {}",
    res.1
  );
  assert!(
    !res.1.contains("/pub/secret"),
    "REPORT log-report leaked /pub/secret: {}",
    res.1
  );
}

fn assert_dav_per_path_deny_checks(daemon: &crate::Daemon, name: &str) {
  let stranger = basic("bob", STRANGER_TOKEN);
  let update_root = dav_update_report_body(&format!("/{name}"));
  let update_secret = dav_update_report_body(&format!("/{name}/secret"));
  let me_target = format!("/{name}/!svn/me");
  let vcc_target = format!("/{name}/!svn/vcc/default");
  let root_target = format!("/{name}");
  let pub_target = format!("/{name}/pub");
  let (me_res, vcc_res, ls_root, ls_pub, log_res) = crate::test_store::run_on_first(
    daemon,
    move || async move {
      let auth = [stranger.as_str()];
      let depth1 = [stranger.as_str(), "Depth: 1"];
      let me = run_live_dav("REPORT", &me_target, &auth, update_root.as_bytes()).await;
      let vcc = run_live_dav("REPORT", &vcc_target, &auth, update_secret.as_bytes()).await;
      let pf_root = run_live_dav("PROPFIND", &root_target, &depth1, PROPFIND_KIND).await;
      let pf_pub = run_live_dav("PROPFIND", &pub_target, &depth1, PROPFIND_KIND).await;
      let log = run_live_dav("REPORT", &root_target, &auth, LOG_EVERY).await;
      (me, vcc, pf_root, pf_pub, log)
    },
    std::time::Duration::from_secs(60),
  );
  assert_dav_checkout_hides_denied(&me_res);
  assert_dav_vcc_denied_root(&vcc_res);
  assert_dav_propfind_hides_secret(&ls_root, "/pub/", "PROPFIND /");
  assert_dav_propfind_hides_secret(&ls_pub, "ok.txt", "PROPFIND /pub");
  assert_dav_log_hides_denied(&log_res);
}

fn assert_dav_subpath_allow_and_disjoint(daemon: &crate::Daemon, name: &str) {
  let stranger = basic("bob", STRANGER_TOKEN);
  let update_trunk = format!(
    "<S:update-report send-all=\"true\" xmlns:S=\"svn:\">\
     <S:src-path>/{name}/trunk</S:src-path>\
     <S:target-revision>1</S:target-revision>\
     <S:entry rev=\"1\" start-empty=\"true\"></S:entry>\
     </S:update-report>"
  );
  let trunk_target = format!("/{name}/trunk");
  let me_target = format!("/{name}/!svn/me");
  let (opt_res, co_res) = crate::test_store::run_on_first(
    daemon,
    move || async move {
      let auth = [stranger.as_str()];
      let opt = run_live_dav("OPTIONS", &trunk_target, &auth, b"").await;
      let co = run_live_dav("REPORT", &me_target, &auth, update_trunk.as_bytes()).await;
      (opt, co)
    },
    std::time::Duration::from_secs(60),
  );
  assert_eq!(opt_res.0, 200, "OPTIONS /trunk under Allow Read /trunk");
  assert_eq!(co_res.0, 200, "REPORT !svn/me /trunk: {}", co_res.1);
  assert!(
    co_res.1.contains("ok.txt"),
    "trunk checkout missing ok.txt: {}",
    co_res.1
  );
}

/// Round-4 Blocker 2 (HTTP WebDAV per-path `Deny Read` through real `ServedRecords`): Do serve
/// `REPORT /!svn/me` (`svn checkout`), `REPORT /!svn/vcc/default` targeting `/secret`,
/// `PROPFIND Depth: 1` on `/` and `/pub`, and `REPORT log-report` under `[Allow Read / Anyone,
/// Deny Read /secret STRANGER, Deny Read /pub/secret STRANGER]`, then test `[Deny Read /,
/// Allow Read /trunk]` checkout of `/trunk` and disjoint `[Allow Read /pub, Allow Submit /trunk]`
/// `POST /!svn/me`. Expect: zero denied paths or secret bytes leak on any DAV verb, subpath
/// `/trunk` checkout succeeds, and disjoint `POST /!svn/me` is refused `403`.
#[test]
fn round4_blocker2_http_dav_reports_and_propfind_enforce_per_path_deny_read() {
  use slates_ipc::protocol::{PolicyEffect, PolicyEntry, PolicyRight, PolicyWho};
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let name = "davdeny";
  let id = crate::test_store::run_on_first(
    &daemon,
    move || async move {
      let asked = (
        name,
        slates_ipc::protocol::VcsTool::Subversion,
        slates_ipc::protocol::ObjectFormat::Revisions,
        "main",
      );
      let (id, _) = crate::test_store::created(&daemon_owner(), asked).await;
      let committed = crate::repository::svn_commit::tests::commit(
        (&daemon_owner(), id),
        dav_deny_revision(),
        None,
      )
      .await;
      assert!(committed.is_ok(), "{committed:?}");
      id
    },
    std::time::Duration::from_secs(60),
  );
  let _ = id;
  let only_stranger = PolicyWho::AnyOf {
    entries: vec![PolicyEntry::Is {
      principal: slates_ipc::protocol::Principal::Uid {
        host: 0,
        uid: STRANGER,
      },
    }],
  };
  set_repo_policy_on_first(
    &daemon,
    name,
    0,
    vec![
      ipc_path_rule(
        "/",
        PolicyRight::Read,
        PolicyEffect::Allow,
        PolicyWho::Anyone,
      ),
      ipc_path_rule(
        "/secret",
        PolicyRight::Read,
        PolicyEffect::Deny,
        only_stranger.clone(),
      ),
      ipc_path_rule(
        "/pub/secret",
        PolicyRight::Read,
        PolicyEffect::Deny,
        only_stranger.clone(),
      ),
    ],
  );
  assert_dav_per_path_deny_checks(&daemon, name);

  set_repo_policy_on_first(
    &daemon,
    name,
    1,
    vec![
      ipc_path_rule(
        "/",
        PolicyRight::Read,
        PolicyEffect::Deny,
        only_stranger.clone(),
      ),
      ipc_path_rule(
        "/trunk",
        PolicyRight::Read,
        PolicyEffect::Allow,
        only_stranger.clone(),
      ),
    ],
  );
  assert_dav_subpath_allow_and_disjoint(&daemon, name);

  set_repo_policy_on_first(
    &daemon,
    name,
    2,
    vec![
      ipc_path_rule(
        "/",
        PolicyRight::Read,
        PolicyEffect::Deny,
        only_stranger.clone(),
      ),
      ipc_path_rule(
        "/pub",
        PolicyRight::Read,
        PolicyEffect::Allow,
        only_stranger.clone(),
      ),
      ipc_path_rule(
        "/",
        PolicyRight::Submit,
        PolicyEffect::Deny,
        only_stranger.clone(),
      ),
      ipc_path_rule(
        "/trunk",
        PolicyRight::Submit,
        PolicyEffect::Allow,
        only_stranger,
      ),
    ],
  );
  let stranger = basic("bob", STRANGER_TOKEN);
  let post_target = format!("/{name}/!svn/me");
  let (post_status, _) = crate::test_store::run_on_first(
    &daemon,
    move || async move {
      run_live_dav(
        "POST",
        &post_target,
        &[stranger.as_str()],
        b"( create-txn ( ) )",
      )
      .await
    },
    std::time::Duration::from_secs(60),
  );
  daemon.stop();
  assert_eq!(
    post_status, 403,
    "disjoint Read/Submit POST /!svn/me must be 403"
  );
}

/// Single-threaded slot holding the active DAV commit transaction for `MergeHookStores`.
type HeldDavCommit = std::cell::RefCell<Option<(String, slates_svn::dav::commit::DavCommit)>>;

/// Pure in-memory `SvnStores` for HTTP `MERGE` pre-commit ordering tests (zero `TestStore`/daemon).
#[derive(Clone)]
struct MergeHookStores {
  records: &'static slates_svn::migration::Repository,
  commit: &'static HeldDavCommit,
}

impl MergeHookStores {
  fn build() -> Self {
    let mut records = slates_svn::migration::Repository::new(
      slates_svn::dump::Uuid::parse(UUID.as_bytes()).unwrap(),
      DATE.as_bytes(),
      slates_svn::migration::Room {
        entries: 1 << 10,
        bytes: 1 << 20,
      },
    )
    .unwrap();
    let seed = slates_svn::commit::revision::Revision {
      head: 0,
      record: RevisionRecord {
        transaction: "0-1".to_owned(),
        changes: vec![
          added_row("/pub", NodeKind::Directory, None),
          added_row("/pub/ok.txt", NodeKind::File, None),
          added_row("/pub/secret", NodeKind::Directory, None),
          added_row("/pub/secret/vault.txt", NodeKind::File, None),
          added_row("/trunk", NodeKind::Directory, None),
          added_row("/trunk/ok.txt", NodeKind::File, None),
        ],
      },
      properties: BTreeMap::from([
        (b"svn:author".to_vec(), b"alice".to_vec()),
        (b"svn:date".to_vec(), DATE.as_bytes().to_vec()),
        (b"svn:log".to_vec(), b"seed".to_vec()),
      ]),
      contents: BTreeMap::new(),
      stored: BTreeMap::new(),
      lock_entries: Vec::new(),
    };
    records.commit(seed).unwrap();
    Self {
      records: Box::leak(Box::new(records)),
      commit: Box::leak(Box::new(std::cell::RefCell::new(None))),
    }
  }
}

impl SvnStores for MergeHookStores {
  type Repository<'a> = crate::vcs_store::svn_texts::SvnView<'a>;
  type Hold = RepositoryId;

  fn holds(&self, id: RepositoryId) -> bool {
    id == REPO
  }

  fn hold(&mut self, id: RepositoryId) -> Option<RepositoryId> {
    self.holds(id).then_some(id)
  }

  fn lend_held<T>(
    &mut self,
    hold: &RepositoryId,
    principal: &Principal,
    step: impl FnOnce(&Lent<'_, Self::Repository<'_>>) -> T,
  ) -> Option<T> {
    self.lend(*hold, principal, step)
  }

  fn lend<T>(
    &mut self,
    id: RepositoryId,
    _principal: &Principal,
    step: impl FnOnce(&Lent<'_, Self::Repository<'_>>) -> T,
  ) -> Option<T> {
    if id != REPO {
      return None;
    }
    let texts = crate::vcs_store::svn_texts::HeadTexts {
      green: None,
      store: None,
      records: self.records,
      pieces: None,
    };
    let view = slates_svn::migration::WithTexts {
      records: self.records,
      texts: &texts,
    };
    Some(step(&Lent {
      index: self.records.index(),
      repository: &view,
      uuid: UUID,
      locks: self.records.locks(),
      rules: None,
      now: NOW,
    }))
  }

  fn begin_commit(
    &mut self,
    id: RepositoryId,
    _principal: &Principal,
    setup_of: impl FnOnce(String, AprTime) -> slates_svn::commit::drive::Setup,
  ) -> Option<
    Result<(String, Option<slates_svn::commit::drive::Due>), slates_svn::dav::locking::HttpAnswer>,
  > {
    if id != REPO {
      return None;
    }
    let name = "1-1".to_owned();
    let texts = Box::new(slates_svn::commit::drive::HeldTexts::new());
    let setup = setup_of(name.clone(), NOW);
    let head = crate::vcs_store::svn_texts::HeadTexts {
      green: None,
      store: None,
      records: self.records,
      pieces: None,
    };
    let view = slates_svn::migration::WithTexts {
      records: self.records,
      texts: &head,
    };
    let world = slates_svn::dav::commit::World {
      index: self.records.index(),
      green: &view,
      locks: self.records.locks(),
    };
    let (commit, due) = match slates_svn::dav::commit::DavCommit::begin_hooked(setup, &world, texts)
    {
      Ok(begun) => begun,
      Err(refused) => return Some(Err(refused)),
    };
    *self.commit.borrow_mut() = Some((name.clone(), commit));
    Some(Ok((name, due)))
  }

  fn on_commit<T>(
    &mut self,
    id: RepositoryId,
    _principal: &Principal,
    name: &str,
    step: impl FnOnce(
      &mut slates_svn::dav::commit::DavCommit,
      &slates_svn::dav::commit::World<'_, crate::vcs_store::svn_texts::SvnView<'_>>,
    ) -> T,
  ) -> Option<Option<T>> {
    if id != REPO {
      return None;
    }
    let mut slot = self.commit.borrow_mut();
    let Some((held_name, commit)) = slot.as_mut() else {
      return Some(None);
    };
    if held_name != name {
      return Some(None);
    }
    let head = crate::vcs_store::svn_texts::HeadTexts {
      green: None,
      store: None,
      records: self.records,
      pieces: None,
    };
    let view = slates_svn::migration::WithTexts {
      records: self.records,
      texts: &head,
    };
    let world = slates_svn::dav::commit::World {
      index: self.records.index(),
      green: &view,
      locks: self.records.locks(),
    };
    Some(Some(step(commit, &world)))
  }
}

struct HookedPolicyHeld {
  policy: RepositoryPolicy,
}

impl Repositories for HookedPolicyHeld {
  fn locate(&self, _name: &str) -> Located {
    Located::Here
  }

  fn find(&self, name: &str) -> Option<RepositoryRecord> {
    (name == "repo").then(|| RepositoryRecord {
      index_bits: 0,
      id: REPO,
      name: "repo".to_owned(),
      tool: VcsTool::Subversion,
      format: ObjectFormat::Revisions,
      default_branch: None,
      owner: principal(OWNER),
      policy: self.policy.clone(),
    })
  }
}

fn drive_merge_exchange(
  site: &mut SvnSite<Tokens, HookedPolicyHeld, MergeHookStores>,
  method: &str,
  target: &str,
  headers: &[&str],
  body: &[u8],
) -> (Status, String) {
  let req = request(method, target, headers);
  let decided = ready(site.answer(&head(&req)));
  let Answer::Serve { mut exchange, .. } = decided.answer else {
    panic!("{method} {target}: expected Answer::Serve");
  };
  let mut out = Vec::new();
  ready(async {
    if !body.is_empty() {
      exchange.body(body).await.unwrap();
    }
    exchange.body_end(ClientGone::never()).await.unwrap();
    while exchange.reply(&mut out, ClientGone::never()).await.unwrap() {}
  });
  (exchange.reply_status(), String::from_utf8(out).unwrap())
}

fn run_dav_delete_merge(
  site: &mut SvnSite<Tokens, HookedPolicyHeld, MergeHookStores>,
  auth: &str,
  delete_path: &str,
) -> (Status, String) {
  let (post_status, post_body) = drive_merge_exchange(
    site,
    "POST",
    "/repo/!svn/me",
    &[auth, "Content-Type: application/vnd.svn-skel"],
    b"( create-txn )",
  );
  assert_eq!(post_status, Status::CREATED, "POST !svn/me: {post_body}");
  let (del_status, _) = drive_merge_exchange(
    site,
    "DELETE",
    &format!("/repo/!svn/txr/1-1{delete_path}"),
    &[auth, "X-SVN-Version-Name: 1"],
    b"",
  );
  assert_eq!(del_status, Status::NO_CONTENT);
  let merge_body = b"<?xml version=\"1.0\" encoding=\"utf-8\"?><D:merge xmlns:D=\"DAV:\">\
<D:source><D:href>/repo/!svn/txn/1-1</D:href></D:source><D:no-auto-merge/></D:merge>";
  drive_merge_exchange(
    site,
    "MERGE",
    "/repo",
    &[auth, "Content-Type: text/xml"],
    merge_body,
  )
}

/// Round-5 Suggestion 1 (HTTP `MERGE` authz before `pre-commit` hook): Do configure a repository
/// with `SvnPreCommit` hook, `Allow Read /`, `Allow Submit /`, and `Deny Read /pub/secret`, then
/// submit two HTTP `MERGE` commits as stranger: (1) deleting `/pub` (which contains denied
/// descendant `/pub/secret/vault.txt`, passing per-request `DELETE` head authz) and (2) deleting
/// `/trunk` (all descendants allowed). Expect: (1) is refused `403 Forbidden` (`errcode="170001"`)
/// BEFORE running `SvnPreCommit`; (2) passes `admit_svn_changes` and reaches `SvnPreCommit`
/// (returning `501`/`200007` outside a hook-granted shard).
#[test]
fn round5_suggestion1_http_merge_checks_authz_before_pre_commit_hook() {
  use slates_db::catalog::{
    HookConfig, HookPointName, PolicyEffect, PolicyNames, PolicyRight, PolicyRule, PolicyWho,
  };
  let rule = |pattern: &str, right: PolicyRight, effect: PolicyEffect| PolicyRule {
    names: PolicyNames::Paths,
    pattern: pattern.to_owned(),
    right,
    effect,
    who: PolicyWho::Anyone,
  };
  let policy = RepositoryPolicy {
    version: 1,
    rules: vec![
      rule("/", PolicyRight::Read, PolicyEffect::Allow),
      rule("/", PolicyRight::Submit, PolicyEffect::Allow),
      rule("/pub/secret", PolicyRight::Read, PolicyEffect::Deny),
    ],
    hooks: vec![HookConfig {
      point: HookPointName::SvnPreCommit,
      name: "pre-commit".to_owned(),
      program: "/bin/true".to_owned(),
    }],
    hooks_env: Vec::new(),
    signers: Vec::new(),
    hg_publishing: true,
  };
  let mut site = SvnSite::new(
    REALM.to_owned(),
    ROOM,
    Tokens,
    HookedPolicyHeld { policy },
    MergeHookStores::build(),
  );
  let stranger = basic("bob", STRANGER_TOKEN);

  let (denied_status, denied_body) = run_dav_delete_merge(&mut site, &stranger, "/pub");
  assert_eq!(
    denied_status,
    Status::FORBIDDEN,
    "denied subtree MERGE must return 403 before pre-commit hook: {denied_body}"
  );
  assert!(
    denied_body.contains("errcode=\"170001\""),
    "expected NOT_AUTHORIZED 170001, got: {denied_body}"
  );

  let (allowed_status, allowed_body) = run_dav_delete_merge(&mut site, &stranger, "/trunk");
  assert_eq!(
    allowed_status,
    Status::NOT_IMPLEMENTED,
    "allowed subtree MERGE must proceed to SvnPreCommit hook: {allowed_body}"
  );
  assert!(
    allowed_body.contains("errcode=\"200007\""),
    "expected hook refusal 200007 when authorized commit reaches pre-commit: {allowed_body}"
  );
}
