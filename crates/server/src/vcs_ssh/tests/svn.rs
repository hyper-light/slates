//! The tunnel's tests (A-688; §4.17.4 "The wire" (svn+ssh) and piece 12): `svnserve -t` dispatched by the SSH site
//! (`ToolSite` over [`SvnServices`]) and run as the edge runs a command: what it wrote taken whenever it holds any, a
//! step asked whenever it can step, and the client's bytes handed over whenever it waits for them.
//!
//! The oracle is stock `svnserve -t` 1.15.0, two ways. A commit: the recorded `mkdir` connection
//! (`crates/svn/fixtures/commit/`, made through `svnserve -t` by its `make-fixtures.py`, each file's SHA-256 in its
//! `SHA256SUMS`), whose every byte the tunnel writes but the user `commit-info` names, its work landed as the owner's
//! step lands one (A-698). And scripted sessions, recorded on the Mac Studio (macOS arm64, Homebrew's `svnserve` 1.15.0) at
//! 2026-11-19T22:27:07Z and 22:33:56Z: each client's bytes piped to `svnserve -t --tunnel-user=alice -r ROOT`, ROOT
//! holding `repo` (`svnadmin create`, `svnadmin setuuid` to [`UUID`], `svnserve.conf` `anon-access = none` and
//! `auth-access = write`) and `closed` (the same with `auth-access = none`), its standard output, its standard error
//! and its exit status kept. The first batch's bytes are checked against their SHA-256 here ([`RECORDED`]); the
//! second's are the first's parts, their lengths as recorded (207, 392, 415 and 431 bytes of output).

use std::cell::Cell;
use std::future::Future;
use std::task::{Context, Poll, Waker};

use aws_lc_rs::digest::{SHA256, digest};
use slates_db::HostId;
use slates_db::catalog::{
  ObjectFormat, Principal, RepositoryId, RepositoryPolicy, RepositoryRecord, VcsTool,
};
use slates_ssh::command::Command as Asked;
use slates_ssh::userauth::{Holding, KeyHolders, UserKey};
use slates_svn::authz;
use slates_svn::commit::drive::{Green, Work};
use slates_svn::commit::revision::Revision as RevisionToCommit;
use slates_svn::dump::Uuid;
use slates_svn::index::{ContentIdentity, Revision};
use slates_svn::lock::AprTime;
use slates_svn::mergeinfo::Contents;
use slates_svn::migration::{Repository as Records, Room};
use slates_svn::ra_svn::{self, Parsed, Parser};
use slates_svn::reads;
use slates_svn::report::exchange::Budget;
use slates_svn::serve::{Limits, Policy, Sources};

use super::super::site::{
  ClientGone, Command, Dispatched, GIT_DIE_STATUS, Next, NotYet, Refused, SshSite, ToolSite,
};
use super::{EitherCommand, Opening, SvnServices, SvnStores, TunnelBounds};
use crate::repository::svn_commit::{Committed, Uncommitted as Unlanded};
use crate::vcs_http::git::{Located, Repositories};

/// Format: the UUID of the recordings' repository (`make-fixtures.py` `UUID`; the scripted sessions' `setuuid`).
const UUID: &str = "00000000-0000-4000-8000-00000000a617";
/// Format: revision 0's `svn:date`, which no session here shows.
const CREATED: &[u8] = b"2026-11-19T05:05:00.000000Z";
/// Shape: the name of the tests' Subversion repository: the recordings' URL's one segment.
const REPOSITORY: &str = "repo";
/// Shape: a Subversion repository whose store the tests' lender does not hold.
const ELSEWHERE: &str = "elsewhere";
/// Shape: a git repository, which no Subversion URL reaches.
const GIT_REPOSITORY: &str = "r.git";
/// Shape: the id of the repository whose store the lender holds.
const HELD: RepositoryId = RepositoryId { bytes: [1; 16] };
/// Shape: the id of [`ELSEWHERE`].
const UNHELD: RepositoryId = RepositoryId { bytes: [2; 16] };
/// Shape: the id of [`GIT_REPOSITORY`].
const GIT_ID: RepositoryId = RepositoryId { bytes: [3; 16] };
/// Shape: the uid of the repositories' owner.
const OWNER: u32 = 1;
/// Shape: the uid of a principal no rule of their policies names.
const STRANGER: u32 = 2;
/// Shape: a tunnel's bounds, past anything the sessions here hold.
const BOUNDS: TunnelBounds = TunnelBounds {
  limits: Limits {
    item_bytes: 1 << 20,
    report_room: 1 << 20,
    most_open: 64,
  },
  budget: Budget {
    room: 1 << 24,
    steps: 1 << 20,
    quantum: None,
  },
  commit_room: 1 << 30,
};
/// Shape: the room the repository's records are held within: past anything the sessions here hold.
const RECORDS_ROOM: Room = Room {
  entries: 1 << 16,
  bytes: 1 << 20,
};
/// Shape: the bounds a recording is split into its items within: past any item it holds.
const ITEM_BOUNDS: ra_svn::Bounds = ra_svn::Bounds {
  item_bytes: 1 << 20,
  word_bytes: 1 << 10,
  depth: 1 << 4,
  nodes: 1 << 16,
};
/// Shape: the steps a tunnel may take before a test judges it never exits: past any session here.
const MOST_STEPS: usize = 1 << 16;
/// Shape: where the recorded `mkdir` connection's client bytes are split in two, inside its first item.
const SPLIT: usize = 100;

/// The recorded `mkdir` connection's client bytes: stock `svn mkdir -m 'the trunk' svn+fixture://fixture/repo/trunk`.
const MKDIR_CLIENT: &[u8] = include_bytes!("../../../../svn/fixtures/commit/mkdir.1.client.bin");
/// What stock `svnserve -t` wrote on it.
const MKDIR_SERVER: &[u8] = include_bytes!("../../../../svn/fixtures/commit/mkdir.1.server.bin");
/// Shape: the items `svnserve -t` wrote through the commit's start: the greeting, the authentication request,
/// `EXTERNAL`'s success, `repos-info`, the commit's trivial auth request and its success.
const COMMIT_STARTED: usize = 6;
/// How a client ends the edit it gave up (`editorp.c` 1205).
const ABORT_EDIT: &[u8] = b"( abort-edit ( ) ) ";
/// The failure `close-edit` is answered with where the owner's step finds no room (`Uncommitted::answered`'s words for
/// a budget of 0 bytes, `SVN_ERR_FS_GENERAL`).
const NO_ROOM: &[u8] =
  b"( failure ( ( 160000 41:No room for the commit: 0 bytes available 0: 0 ) ) ) ";
/// Shape: the instant stock `svnserve -t` recorded the `mkdir` commit at (its `commit-info`'s date,
/// 2026-11-19T05:05:32.129962Z): the instant the tests' lender lends and lands at.
const NOW: AprTime = AprTime(1_795_064_732_129_962);
/// The `svn:author` stock `svnserve -t` recorded the `mkdir` commit by, as `commit-info` writes it: its tunnel's user.
const RECORDED_AUTHOR: &[u8] = b"( 5:alice )";

/// The capabilities the scripted clients name, a stock 1.15.0 client's.
const CAPABILITIES: &str =
  "edit-pipeline svndiff1 accepts-svndiff2 absent-entries depth mergeinfo log-revprops";
/// The RA client the scripted clients name, a stock 1.15.0 client's.
const RA_CLIENT: &str = "39:SVN/1.15.0 (aarch64-apple-darwin25.6.0)";
/// The URL of the recordings' repository.
const ROOT_URL: &str = "svn+fixture://fixture/repo";
/// The command a scripted client asks once it is established.
const LATEST: &str = "( get-latest-rev ( ) ) ";
/// A commit a scripted client begins, then its `open-root`.
const COMMIT_AND_ROOT: &str =
  "( commit ( 9:the trunk ( ) true ( ( 7:svn:log 9:the trunk ) ) ) ) ( open-root ( ( ) 2:d0 ) ) ";
/// An update a scripted client begins, then its report's first path.
const UPDATE_AND_PATH: &str =
  "( update ( ( 0 ) 0: true unknown false false ) ) ( set-path ( 0: 0 true ( ) infinity ) ) ";

/// Stock `svnserve` 1.15.0's greeting.
const GREETING: &str = concat!(
  "( success ( 2 2 ( ) ( edit-pipeline svndiff1 accepts-svndiff2 absent-entries commit-revprops ",
  "depth log-revprops atomic-revprops partial-replay inherited-props ephemeral-txnprops ",
  "file-revs-reverse list ) ) ) ",
);
/// The authentication request for the recordings' repository: the tunnel's user, its UUID the realm.
const AUTH_REQUEST: &str = "( success ( ( EXTERNAL ) 36:00000000-0000-4000-8000-00000000a617 ) ) ";
/// `EXTERNAL`'s success.
const AUTHENTICATED: &str = "( success ( ) ) ";
/// `repos-info` for the recordings' repository.
const REPOS_INFO: &str = concat!(
  "( success ( 36:00000000-0000-4000-8000-00000000a617 26:svn+fixture://fixture/repo ",
  "( mergeinfo ) ) ) ",
);
/// A command's trivial auth request.
const TRIVIAL_AUTH: &str = "( success ( ( ) 0: ) ) ";
/// A commit's success: its drive takes the editor's commands.
const COMMIT_BEGUN: &str = "( success ( ) ) ";
/// `get-latest-rev`'s answer in a repository at revision 0.
const LATEST_ANSWER: &str = "( success ( 0 ) ) ";
/// A refused `EXTERNAL` naming another user than the tunnel's.
const NOT_THE_USER: &str = "( failure ( 33:Requested username does not match ) ) ";
/// The refusal of a repository closed to the principal.
const NO_ACCESS: &str = "( failure ( ( 170001 36:No access allowed to this repository 0: 0 ) ) ) ";
/// What `svnserve -t` ends with where its client left in the middle of an exchange.
const CLOSED_ERR: &str = "svnserve: E210002: Network connection closed unexpectedly\n";
/// What `svnserve -t` ends with where a repository is closed to the principal.
const NO_ACCESS_ERR: &str = "svnserve: E170001: No access allowed to this repository\n";
/// What the site answers where it cannot serve the tunnel's repository (the site's not-served words).
const NOT_SERVED_ERR: &str = "svnserve: svnserve -t is not served yet\n";
/// What `svnserve -t` ends with on a client at protocol version 3.
const UNSUPPORTED_ERR: &str =
  "svnserve: E210006: Unsupported ra_svn protocol version 3 (supported versions: [2])\n";

/// The recorded sessions of the first batch, each with the SHA-256 of its client's bytes, of `svnserve -t`'s output and
/// of its standard error, as `shasum -a 256` printed them on the Studio.
const RECORDED: [(&str, [&str; 3]); 6] = [
  (
    "latest",
    [
      "76bcebac6ab2d168e76101381e70271b0af80eb47ea9bdd737365f3e74d7ace8",
      "2707f9ddaf3e0fa95acc07c7f6b1fe871d1e7ff1cdc4407191225fe4254bbbec",
      "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    ],
  ),
  (
    "beneath",
    [
      "c5a4111958a753765fe2e3d2c8e2f79f5e5731bc8b93c6942c71b992a2620365",
      "2707f9ddaf3e0fa95acc07c7f6b1fe871d1e7ff1cdc4407191225fe4254bbbec",
      "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    ],
  ),
  (
    "not-the-user",
    [
      "1e6f11c4fc43b8510effa10ab189585aa3f385f806df17520ed79c30831b3fad",
      "576025018de7df52a4d80bb74a6cae48ac63acdbda04c5770e84081cf1092ad7",
      "61c7816303dd15cfc2360c6de6e967954260f887ee80462528b6ae2963d797af",
    ],
  ),
  (
    "no-repository",
    [
      "bb21d9067ac824c3f9435e9deb497c58dc8c28a75dda1c1c15192d992a94c981",
      "eab7aa7512118c37d10cc93028ce2087d7c1b456aab154c36365ab1e691222e1",
      "81d30ad283d5a3b5a3e49716b4889b821505317925d62bc6d03f9056fdbf9dfb",
    ],
  ),
  (
    "bad-url",
    [
      "c8f794fa13b44316274477f1aaac90c6cec547ebd26e98c14ffbced4cef9ec45",
      "07472464b425bf0aea9eaf096f7622b287e9f6c1c4c824f432b562f1095b357f",
      "9f184349014cf97ed4c0728f461b93a453c4f345ce06b217fc2e58a586fb0be3",
    ],
  ),
  (
    "back-path",
    [
      "055990e8e70d53cf5159358893b03cfdec60e4e1fcae0e15770507e1a7d670a5",
      "58fd6831460d94a07445ce5c282ae37108c95b0cf92adad86673e910d2e04559",
      "8f40b74a0825ab3e3b4e547c7cd3b1abd9e82943f6bc11c7f0b6592a76b90db3",
    ],
  ),
];

/// A scripted client's answer to the greeting at protocol `version` with `capabilities`, naming `url` (the recording
/// script's `answer`).
fn answer_with(version: u8, capabilities: &str, url: &str) -> String {
  format!(
    "( {version} ( {capabilities} ) {}:{url} {RA_CLIENT} ( ) ) ",
    url.len()
  )
}

/// A stock client's answer to the greeting naming `url`.
fn answer(url: &str) -> String {
  answer_with(2, CAPABILITIES, url)
}

/// `EXTERNAL` naming `user`: the empty name takes the tunnel's user.
fn external(user: &str) -> String {
  format!("( EXTERNAL ( {}:{user} ) ) ", user.len())
}

/// The session established over the recordings' repository, as `svnserve -t` writes it.
fn established() -> String {
  [GREETING, AUTH_REQUEST, AUTHENTICATED, REPOS_INFO].concat()
}

/// The URL `url` refused as naming no repository: `svnserve -t`'s failure to the client and its standard error.
fn no_repository(url: &str) -> (String, String) {
  let message = format!("No repository found in '{url}'");
  (
    format!(
      "( failure ( ( 210005 {}:{message} 0: 0 ) ) ) ",
      message.len()
    ),
    format!("svnserve: E210005: {message}\n"),
  )
}

/// The recorded `latest` session's client: the greeting answered naming the repository, `EXTERNAL`, `get-latest-rev`.
pub(in crate::vcs_ssh) fn latest_client() -> Vec<u8> {
  [answer(ROOT_URL), external(""), LATEST.to_owned()]
    .concat()
    .into_bytes()
}

/// What `svnserve -t` wrote on the `latest` session.
fn latest_stock() -> String {
  [
    established(),
    TRIVIAL_AUTH.to_owned(),
    LATEST_ANSWER.to_owned(),
  ]
  .concat()
}

/// What the tunnel writes on the `latest` session: `svnserve -t`'s bytes as slates writes them.
pub(in crate::vcs_ssh) fn latest_answer() -> Vec<u8> {
  as_slates_writes(latest_stock().as_bytes())
}

/// `stock`, bytes stock `svnserve` wrote, as slates writes them: its greeting less `svndiff1`, which slates never
/// announces (A-599).
fn as_slates_writes(stock: &[u8]) -> Vec<u8> {
  let word = b"svndiff1 ";
  match stock.windows(word.len()).position(|window| window == word) {
    Some(at) => [&stock[..at], &stock[at + word.len()..]].concat(),
    None => stock.to_vec(),
  }
}

/// `bytes` split into their items, each with its terminating whitespace.
fn items(bytes: &[u8]) -> Vec<&[u8]> {
  let mut found = Vec::new();
  let mut rest = bytes;
  while !rest.is_empty() {
    match Parser::new(ITEM_BOUNDS).parse(rest) {
      Ok(Parsed::Item { consumed, .. }) => {
        found.push(&rest[..consumed]);
        rest = &rest[consumed..];
      }
      other => panic!("no whole item at {}: {other:?}", bytes.len() - rest.len()),
    }
  }
  found
}

/// The principal with uid `uid` on host 0.
fn principal(uid: u32) -> Principal {
  Principal::Uid {
    host: HostId(0),
    uid,
  }
}

/// Holds no keys: these tests decide `exec`s for principals already proven.
#[derive(Clone)]
struct NoKeys;

impl KeyHolders for NoKeys {
  type Principal = Principal;

  fn holding(&mut self, _user: &[u8], _key: &UserKey, _blob: &[u8]) -> Holding<Principal> {
    Holding::Nobody
  }
}

/// The repositories the site finds: [`REPOSITORY`] and [`ELSEWHERE`], Subversion's, and [`GIT_REPOSITORY`], each
/// `owner`'s with no rules.
#[derive(Clone)]
struct Held {
  /// Their owner.
  owner: Principal,
  /// The hooks their policy configures (A-730).
  hooks: Vec<slates_db::catalog::HookConfig>,
}

impl Repositories for Held {
  fn locate(&self, _name: &str) -> Located {
    Located::Here
  }

  fn find(&self, name: &str) -> Option<RepositoryRecord> {
    let (id, tool, format) = match name {
      REPOSITORY => (HELD, VcsTool::Subversion, ObjectFormat::Revisions),
      ELSEWHERE => (UNHELD, VcsTool::Subversion, ObjectFormat::Revisions),
      GIT_REPOSITORY => (GIT_ID, VcsTool::Git, ObjectFormat::Sha1),
      _ => return None,
    };
    Some(RepositoryRecord {
      index_bits: 0,
      id,
      name: name.to_owned(),
      tool,
      format,
      default_branch: None,
      owner: self.owner.clone(),
      policy: RepositoryPolicy {
        version: 1,
        rules: Vec::new(),
        hooks: self.hooks.clone(),
        hooks_env: Vec::new(),
        signers: Vec::new(),
      },
    })
  }
}

/// A Subversion repository's records as the connection reads a green: one version of them, kept for the test's life,
/// with the texts its landings stored, as the content store holds them beside the records.
struct View(&'static Records, &'static StoredTexts);

/// The texts the landings stored, by identity.
type StoredTexts = std::cell::RefCell<std::collections::BTreeMap<ContentIdentity, Vec<u8>>>;

impl View {
  /// `identity`'s stored text from `offset`, when a landing stored it.
  fn stored(&self, identity: &ContentIdentity, offset: u64) -> Option<std::borrow::Cow<'_, [u8]>> {
    let stored = self.1.borrow();
    let text = stored.get(identity)?;
    let from = usize::try_from(offset).ok()?;
    Some(std::borrow::Cow::Owned(text.get(from..)?.to_vec()))
  }
}

impl Contents for View {
  fn entries(&self, directory: &str, revision: Revision) -> Option<Vec<String>> {
    (directory == "/" && revision == 0).then(Vec::new)
  }

  fn mergeinfo_value(&self, _properties: &ContentIdentity) -> Option<&[u8]> {
    None
  }
}

impl reads::Repository for View {
  fn revision_properties(&self, revision: Revision) -> Option<Vec<(&[u8], &[u8])>> {
    let properties = self.0.revision_properties(revision)?;
    Some(
      properties
        .iter()
        .map(|(name, value)| (name.as_slice(), value.as_slice()))
        .collect(),
    )
  }

  fn properties(&self, identity: &ContentIdentity) -> Option<Vec<(&[u8], &[u8])>> {
    Green::properties(self.0, identity)
  }

  fn text(&self, identity: &ContentIdentity, offset: u64) -> Option<std::borrow::Cow<'_, [u8]>> {
    reads::Repository::text(self.0, identity, offset).or_else(|| self.stored(identity, offset))
  }
}

impl Green for View {
  fn text(&self, identity: &ContentIdentity) -> Option<std::borrow::Cow<'_, [u8]>> {
    Green::text(self.0, identity).or_else(|| self.stored(identity, 0))
  }

  fn properties(&self, identity: &ContentIdentity) -> Option<Vec<(&[u8], &[u8])>> {
    Green::properties(self.0, identity)
  }

  fn name_text(&self, text: &[u8]) -> ContentIdentity {
    Green::name_text(self.0, text)
  }

  fn name_properties(&self, properties: &[(Vec<u8>, Vec<u8>)]) -> ContentIdentity {
    Green::name_properties(self.0, properties)
  }
}

/// A Subversion repository as `CreateRepository` leaves one (A-575), the owner's records, and what its lender did:
/// each version of the records kept for the test's life, the current one lent, a landed work making the next.
struct Created {
  /// The records lent now.
  current: Cell<&'static Records>,
  /// How many works were landed or refused.
  commits: Cell<u64>,
  /// The texts landed works stored.
  stored: StoredTexts,
}

/// What the recordings' repository's sessions open with (`anon-access = none`, `auth-access = write`): its UUID its
/// realm, mergeinfo kept, the tunnel's user offered, no keys held.
const OPENING: Opening<'static> = Opening {
  uuid: UUID.as_bytes(),
  realm: UUID.as_bytes(),
  mergeinfo: true,
  anonymous: false,
  authenticated: true,
  cram_md5: false,
};

/// What its `svnserve.conf` gives a session: nothing anonymous, write authenticated, every path (no authorization
/// file).
const POLICY: Policy<'static> = Policy {
  anonymous: authz::Access::None,
  authenticated: authz::Access::Write,
  authz: None,
  filesystem: REPOSITORY,
};

/// Lends the store of [`REPOSITORY`] alone, at [`NOW`], and lands a work as the owner's step does on its records
/// (`Repository::check_work` with the commit's locks, then `apply`), or refuses every work for want of room.
#[derive(Clone, Copy)]
struct Lends {
  /// The repository, kept for the test's life.
  created: &'static Created,
  /// Whether every work is refused for want of room.
  full: bool,
}

impl Lends {
  /// A lender of a repository just created.
  fn created() -> Lends {
    let uuid = Uuid::parse(UUID.as_bytes()).expect("the recordings' UUID");
    let records = Records::new(uuid, CREATED, RECORDS_ROOM).expect("revision 0's records");
    Lends {
      created: Box::leak(Box::new(Created {
        current: Cell::new(Box::leak(Box::new(records))),
        commits: Cell::new(0),
        stored: StoredTexts::default(),
      })),
      full: false,
    }
  }

  /// The records' youngest revision now.
  fn youngest(self) -> Revision {
    self.created.current.get().youngest()
  }
}

/// `work` landed on the next version of `records` at `now`, as the owner's step checks and applies it.
fn landed(
  records: &Records,
  work: Work,
  now: AprTime,
  texts: &dyn slates_svn::commit::drive::TextSink,
  stored: &StoredTexts,
) -> Result<(Records, Committed), Unlanded> {
  let locks = work.locks.clone();
  let revision = RevisionToCommit::of_work(work, now).map_err(Unlanded::Step)?;
  for (identity, len) in &revision.stored {
    let len = usize::try_from(*len).unwrap_or(usize::MAX);
    if let Some(text) = texts.sealed_piece(identity, 0, len) {
      stored.borrow_mut().insert(*identity, text);
    }
  }
  let (date, author) = (
    revision.date().map(str::to_owned),
    revision.author().map(str::to_owned),
  );
  let mut next = records.clone();
  let checked = next
    .check_work(revision, &locks, now)
    .map_err(Unlanded::Step)?;
  let number = next.apply(checked).map_err(Unlanded::Step)?;
  let committed = Committed {
    revision: number,
    date,
    author,
  };
  Ok((next, committed))
}

impl SvnStores for Lends {
  type Green<'a> = View;

  fn holds(&self, id: RepositoryId) -> bool {
    id == HELD
  }

  fn opening<T>(&mut self, id: RepositoryId, step: impl FnOnce(&Opening<'_>) -> T) -> Option<T> {
    self.holds(id).then(|| step(&OPENING))
  }

  async fn commit(
    &mut self,
    id: RepositoryId,
    _principal: &Principal,
    work: Work,
    texts: &dyn slates_svn::commit::drive::TextSink,
  ) -> Option<Result<Committed, Unlanded>> {
    if !self.holds(id) {
      return None;
    }
    let created = self.created;
    created.commits.set(created.commits.get() + 1);
    if self.full {
      return Some(Err(Unlanded::Budget { available: 0 }));
    }
    Some(
      landed(created.current.get(), work, NOW, texts, &created.stored).map(|(next, committed)| {
        created.current.set(Box::leak(Box::new(next)));
        committed
      }),
    )
  }

  fn land_record(
    &mut self,
    id: RepositoryId,
    _principal: &Principal,
    record: slates_svn::commit::record::Record,
  ) -> Option<Result<(), Unlanded>> {
    if !self.holds(id) {
      return None;
    }
    let mut next = self.created.current.get().clone();
    Some(match next.commit_record(record) {
      Ok(()) => {
        self.created.current.set(Box::leak(Box::new(next)));
        Ok(())
      }
      Err(refusal) => Err(Unlanded::Step(refusal)),
    })
  }

  fn lend<T>(
    &mut self,
    id: RepositoryId,
    texts: &dyn slates_svn::commit::drive::TextSink,
    step: impl FnOnce(&Sources<'_, View>) -> T,
  ) -> Option<T> {
    if !self.holds(id) {
      return None;
    }
    let records = self.created.current.get();
    Some(step(&Sources {
      index: records.index(),
      repository: &View(records, &self.created.stored),
      locks: records.locks(),
      now: NOW,
      policy: POLICY,
      texts,
    }))
  }
}

/// The SSH site with Subversion's tunnel beside services that serve nothing yet.
type TunnelSite<K> = ToolSite<K, Held, SvnServices<NotYet, Held, Lends>>;

/// The site with Subversion's tunnel over [`Held`]'s repositories and `lends`, `keys` its lookup and `owner` the
/// repositories' owner, every other service not served.
fn site_over<K>(keys: K, owner: Principal, lends: Lends) -> TunnelSite<K> {
  let held = Held {
    owner,
    hooks: Vec::new(),
  };
  let services = SvnServices::new(NotYet, held.clone(), lends, BOUNDS);
  ToolSite::new(keys, held, services)
}

/// [`site_over`] a fresh repository; and its lender, to read what it did.
fn site<K>(keys: K, owner: Principal) -> (TunnelSite<K>, Lends) {
  let lends = Lends::created();
  (site_over(keys, owner, lends), lends)
}

/// The site the edge's tests serve a tunnel through: [`site`]'s, its key holders `keys`, which the edge sends with a
/// connection it moves to another shard (A-651 part 4).
pub(in crate::vcs_ssh) fn tunnel_site<K>(
  keys: K,
  owner: Principal,
) -> impl SshSite<Keys = K> + 'static
where
  K: KeyHolders<Principal = Principal> + Clone + 'static,
{
  site(keys, owner).0
}

/// What a tunnel gave its client.
#[derive(Debug, Default, PartialEq, Eq)]
struct Answered {
  /// Standard output.
  out: Vec<u8>,
  /// Standard error.
  err: Vec<u8>,
  /// The exit status.
  status: Option<u32>,
}

/// What a tunnel gave its client, and the client's bytes it consumed, which the edge gives back to the client's window.
type Ended = (Answered, usize);

impl Answered {
  /// A tunnel that wrote `out` and `err` and exited `status`.
  fn of(out: &[u8], err: &str, status: u32) -> Answered {
    Answered {
      out: out.to_vec(),
      err: err.as_bytes().to_vec(),
      status: Some(status),
    }
  }
}

/// `future`'s output, polled once: a tunnel's steps never wait.
fn ready<F: Future>(future: F) -> F::Output {
  let mut future = std::pin::pin!(future);
  match future
    .as_mut()
    .poll(&mut Context::from_waker(Waker::noop()))
  {
    Poll::Ready(output) => output,
    Poll::Pending => panic!("a step that waits"),
  }
}

/// Takes what `command` wrote into `answered`, standard error first, as the channel takes it.
fn take<C: Command>(command: &mut C, answered: &mut Answered) {
  let (err, out) = command.output();
  answered.err.extend_from_slice(err);
  answered.out.extend_from_slice(out);
  let taken = (err.len(), out.len());
  command.sent(taken.0, taken.1);
}

/// Hands `command` the client's next run, or ends its input when there is none.
fn hand_over<C: Command>(command: &mut C, run: Option<&&[u8]>) {
  match run {
    Some(run) => command.input(run).expect("the tunnel holds its input"),
    None => command.input_end(),
  }
}

/// Runs `command` as the edge runs one: what it wrote taken whenever it holds any; a step asked whenever it can step;
/// the next of `runs` handed over whenever it waits for input, and the input ended once they are spent; until it
/// exits, within [`MOST_STEPS`].
fn drive<C: Command>(command: &mut C, runs: &[&[u8]]) -> Ended {
  let mut answered = Answered::default();
  let mut consumed = 0;
  let mut runs = runs.iter();
  let mut next = Next::More;
  for _ in 0..MOST_STEPS {
    take(command, &mut answered);
    match next {
      Next::Exited(status) => {
        answered.status = Some(status);
        return (answered, consumed);
      }
      Next::Input => hand_over(command, runs.next()),
      Next::More => {}
    }
    let stepped = ready(command.step(ClientGone::never()))
      .expect("a tunnel ends as svnserve ends, never failed");
    consumed += stepped.consumed;
    next = stepped.next;
  }
  panic!("a tunnel that never exits: {answered:?}");
}

/// `svnserve -t` dispatched for `asker` through `site` and run over `runs`; a refused `exec`'s words and status as the
/// edge sends them.
fn tunnel<S: SshSite>(site: &mut S, asker: &Principal, runs: &[&[u8]]) -> Ended {
  match site.dispatch(asker, &Asked::Subversion, None) {
    Dispatched::Run(mut command) => drive(&mut command, runs),
    Dispatched::Refuse(refused) => (
      Answered {
        err: refused.stderr,
        status: Some(refused.exit_status),
        ..Answered::default()
      },
      0,
    ),
  }
}

/// A scripted session's client bytes run whole, as the owner, through a fresh site: what the tunnel gave its client.
fn scripted(client: &str) -> Ended {
  let (mut site, _) = site(NoKeys, principal(OWNER));
  tunnel(&mut site, &principal(OWNER), &[client.as_bytes()])
}

/// What `svnserve -t` was recorded answering: `stock` output as slates writes it, `err` and `status`.
fn recorded(stock: &str, err: &str, status: u32) -> Answered {
  Answered::of(&as_slates_writes(stock.as_bytes()), err, status)
}

/// The SHA-256 of `bytes` in lower-case hexadecimal, as `shasum -a 256` prints it.
fn sha256(bytes: &[u8]) -> String {
  digest(&SHA256, bytes)
    .as_ref()
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect()
}

/// The first batch's sessions as these tests write them: each one's client, `svnserve -t`'s output and its error.
fn first_batch() -> [(&'static str, [String; 3]); 6] {
  let (none_failure, none_err) = no_repository("svn+fixture://fixture/none");
  [
    (
      "latest",
      [
        String::from_utf8_lossy(&latest_client()).into_owned(),
        latest_stock(),
        String::new(),
      ],
    ),
    (
      "beneath",
      [
        [
          answer(&format!("{ROOT_URL}/trunk/project")),
          external(""),
          LATEST.to_owned(),
        ]
        .concat(),
        latest_stock(),
        String::new(),
      ],
    ),
    (
      "not-the-user",
      [
        [answer(ROOT_URL), external("git")].concat(),
        [GREETING, AUTH_REQUEST, NOT_THE_USER].concat(),
        CLOSED_ERR.to_owned(),
      ],
    ),
    (
      "no-repository",
      [
        answer("svn+fixture://fixture/none"),
        [GREETING.to_owned(), none_failure].concat(),
        none_err,
      ],
    ),
    (
      "bad-url",
      [
        answer("http://fixture/repo"),
        [GREETING, BAD_URL].concat(),
        BAD_URL_ERR.to_owned(),
      ],
    ),
    (
      "back-path",
      [
        answer("svn+fixture://fixture/repo/../closed"),
        [GREETING, BAD_PATH].concat(),
        BAD_PATH_ERR.to_owned(),
      ],
    ),
  ]
}

/// A URL of another scheme refused.
const BAD_URL: &str =
  "( failure ( ( 125002 55:Non-svn URL passed to svn server: 'http://fixture/repo' 0: 0 ) ) ) ";
/// What `svnserve -t` ends with on it.
const BAD_URL_ERR: &str =
  "svnserve: E125002: Non-svn URL passed to svn server: 'http://fixture/repo'\n";
/// A URL climbing out with `..` refused.
const BAD_PATH: &str = "( failure ( ( 125001 34:Couldn't determine repository path 0: 0 ) ) ) ";
/// What `svnserve -t` ends with on it.
const BAD_PATH_ERR: &str = "svnserve: E125001: Couldn't determine repository path\n";

/// A-688 (the module doc's oracle). Do: write the first batch's sessions as these tests write them and digest each
/// client's, output's and error's bytes. Expect: the SHA-256 the Studio printed for each, so every session below runs
/// the recorded client and is held to the recorded answer.
#[test]
fn the_scripted_sessions_are_the_recorded_ones() {
  let written =
    first_batch().map(|(name, parts)| (name, parts.map(|part| sha256(part.as_bytes()))));
  let expected = RECORDED.map(|(name, digests)| (name, digests.map(str::to_owned)));
  assert_eq!(written, expected);
}

/// `bytes` with the first `from` in them made `to`.
fn replaced(bytes: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
  match bytes.windows(from.len()).position(|window| window == from) {
    Some(at) => [&bytes[..at], to, &bytes[at + from.len()..]].concat(),
    None => bytes.to_vec(),
  }
}

/// A-698 (A-655 part 2b's owner step behind A-656's connection; §4.17.4 items 3.2 to 3.5). Do: as the repository's
/// owner, dispatch `svnserve -t` through the site and hand its tunnel the recorded `mkdir` connection's client bytes,
/// split inside an item, then end the input. Expect: stock `svnserve -t`'s every byte, its greeting less `svndiff1`
/// and its `commit-info` naming the tunnel's user, the principal the key proved, where the recording named `alice`:
/// revision 1, dated as the step landed it; nothing on standard error and exit 0, as `svnserve -t` ended it; every
/// byte consumed; the work landed once and the repository at revision 1.
#[test]
fn a_stock_commit_lands_through_the_owners_step_as_svnserve_t_answered_it() {
  let (mut site, lends) = site(NoKeys, principal(OWNER));
  let (head, tail) = MKDIR_CLIENT.split_at(SPLIT);
  let ended = tunnel(&mut site, &principal(OWNER), &[head, tail]);
  let named = crate::lfs::owner_name(&principal(OWNER));
  let author = format!("( {}:{named} )", named.len());
  let stock = replaced(MKDIR_SERVER, RECORDED_AUTHOR, author.as_bytes());
  let answered = Answered::of(&as_slates_writes(&stock), "", 0);
  assert_eq!(ended, (answered, MKDIR_CLIENT.len()));
  assert_eq!((lends.created.commits.get(), lends.youngest()), (1, 1));
}

/// A-698 (the step's refusal as `close-edit`'s failure; `editorp.c` 1149-1208). Do: through a lender whose step finds
/// no room, hand the tunnel the recorded `mkdir` connection's client bytes, then `abort-edit`, as a client ends the
/// edit it gave up, then end the input. Expect: stock `svnserve -t`'s bytes item for item through the commit's start,
/// then `close-edit` refused in the step's words (160000, no room); nothing on standard error and exit 0; every byte
/// consumed; the work refused once and the repository still at revision 0.
#[test]
fn a_work_the_owners_step_refuses_is_answered_as_close_edits_failure() {
  let lends = Lends {
    full: true,
    ..Lends::created()
  };
  let mut site = site_over(NoKeys, principal(OWNER), lends);
  let ended = tunnel(&mut site, &principal(OWNER), &[MKDIR_CLIENT, ABORT_EDIT]);
  let stock = items(MKDIR_SERVER);
  let started = as_slates_writes(&stock[..COMMIT_STARTED].concat());
  let consumed = MKDIR_CLIENT.len() + ABORT_EDIT.len();
  let answered = Answered::of(&[started, NO_ROOM.to_vec()].concat(), "", 0);
  assert_eq!(ended, (answered, consumed));
  assert_eq!((lends.created.commits.get(), lends.youngest()), (1, 0));
}

/// A-688 (`find_repos`: the repository is the longest run of the URL's leading segments that names one). Do: run the
/// recorded `latest` session, then `beneath`, whose URL names a path beneath the repository's root. Expect: each
/// answered as `svnserve -t` answered it, `repos-info` naming the repository's root, the youngest revision 0; nothing
/// on standard error and exit 0, the client having left between commands.
#[test]
fn a_scripted_session_is_answered_as_svnserve_t_answers_it() {
  let beneath = [
    answer(&format!("{ROOT_URL}/trunk/project")),
    external(""),
    LATEST.to_owned(),
  ]
  .concat();
  let latest = String::from_utf8_lossy(&latest_client()).into_owned();
  assert_eq!(
    (scripted(&latest), scripted(&beneath)),
    (
      (recorded(&latest_stock(), "", 0), latest.len()),
      (recorded(&latest_stock(), "", 0), beneath.len())
    )
  );
}

/// A-688 (§4.17.4: over slates' SSH server "the principal the one the client's key proved"; `EXTERNAL` takes the
/// tunnel's user). Do: run the `latest` session with `EXTERNAL` naming the principal as slates names it, then the
/// recorded `not-the-user` session naming `git`, the login every git and svn+ssh client sends. Expect: the first
/// answered as the session with the empty name; the second refused "Requested username does not match", then, the
/// client gone inside the handshake, ended E210002 with exit 1, as `svnserve -t` ends it: the login names no one.
#[test]
fn the_tunnels_user_is_the_principal_the_key_proved_never_the_login() {
  let named = crate::lfs::owner_name(&principal(OWNER));
  let the_user = [answer(ROOT_URL), external(&named), LATEST.to_owned()].concat();
  let the_login = [answer(ROOT_URL), external("git")].concat();
  let refused = [GREETING, AUTH_REQUEST, NOT_THE_USER].concat();
  assert_eq!(
    (scripted(&the_user).0, scripted(&the_login).0),
    (
      recorded(&latest_stock(), "", 0),
      recorded(&refused, CLOSED_ERR, 1)
    )
  );
}

/// A-688 (`find_repos`'s refusals, written to the client and ended with). Do: run sessions naming no repository, a URL
/// of another scheme, a URL climbing out with `..`, and a git repository's name. Expect: the recorded failures and
/// standard errors, exit 1 each, the git repository's refused as naming no repository (no Subversion repository holds
/// it).
#[test]
fn a_url_naming_no_subversion_repository_is_refused_as_svnserve_t_refuses_it() {
  let refused = |url: &str, failure: String, err: String| {
    let stock = [GREETING.to_owned(), failure].concat();
    (scripted(&answer(url)).0, recorded(&stock, &err, 1))
  };
  let none = no_repository("svn+fixture://fixture/none");
  let git_url = format!("svn+fixture://fixture/{GIT_REPOSITORY}");
  let git = no_repository(&git_url);
  let sessions = [
    refused("svn+fixture://fixture/none", none.0, none.1),
    refused(
      "http://fixture/repo",
      BAD_URL.to_owned(),
      BAD_URL_ERR.to_owned(),
    ),
    refused(
      "svn+fixture://fixture/repo/../closed",
      BAD_PATH.to_owned(),
      BAD_PATH_ERR.to_owned(),
    ),
    refused(&git_url, git.0, git.1),
  ];
  let (ran, expected): (Vec<_>, Vec<_>) = sessions.into_iter().unzip();
  assert_eq!(ran, expected);
}

/// A-688 (A-597's `vcs_policy::reaches`, as the site judges a Mercurial session; `svnserve`'s words for a repository
/// closed to the user). Do: as a principal the repository's policy lets reach it for no right, run a session naming it.
/// Expect: "No access allowed to this repository" (170001) written and ended with, exit 1, as `svnserve -t` refuses a
/// repository whose `auth-access` is `none` (the recorded `no-access` session's words).
#[test]
fn a_principal_the_policy_keeps_out_is_refused_as_svnserve_t_refuses_a_closed_repository() {
  let client = answer(ROOT_URL);
  let (mut site, _) = site(NoKeys, principal(OWNER));
  let (answered, _) = tunnel(&mut site, &principal(STRANGER), &[client.as_bytes()]);
  let stock = [GREETING, NO_ACCESS].concat();
  assert_eq!(answered, recorded(&stock, NO_ACCESS_ERR, 1));
}

/// A-688 (the module doc: a repository whose store is not held here is not served). Do: run a session naming a
/// Subversion repository whose store the lender does not hold. Expect: nothing but the greeting written, and the site's
/// not-served words on standard error with svnserve's failure, 1.
#[test]
fn a_repository_whose_store_is_not_held_here_is_not_served() {
  let client = answer(&format!("svn+fixture://fixture/{ELSEWHERE}"));
  assert_eq!(scripted(&client).0, recorded(GREETING, NOT_SERVED_ERR, 1));
}

/// A-688 (the tunnel's endings, as `svnserve -t` 1.15.0 ended each recorded session of the second batch). Do: run a
/// client that leaves inside the handshake, one at protocol version 3, one without `edit-pipeline`, one that sends a
/// malformed item, one that leaves inside a command's item, one inside a report, one inside a commit's edit. Expect:
/// what `svnserve -t` wrote and ended with for each: E210002, E210006 twice in the handshake's words, E210004, a clean
/// 0, E210002 twice; exit 1 for each error.
#[test]
fn the_tunnel_ends_as_svnserve_t_ends_each_session() {
  let base = [answer(ROOT_URL), external("")].concat();
  let after = |bytes: &str| [base.clone(), bytes.to_owned()].concat();
  let opened = [GREETING, AUTH_REQUEST].concat();
  let reporting = [established(), TRIVIAL_AUTH.to_owned()].concat();
  let editing = [reporting.clone(), COMMIT_BEGUN.to_owned()].concat();
  let rows = [
    (answer(ROOT_URL), opened, CLOSED_ERR, 1),
    (
      answer_with(3, CAPABILITIES, ROOT_URL),
      GREETING.to_owned(),
      UNSUPPORTED_ERR,
      1,
    ),
    (
      answer_with(
        2,
        CAPABILITIES.trim_start_matches("edit-pipeline "),
        ROOT_URL,
      ),
      GREETING.to_owned(),
      "svnserve: E210006: Missing edit-pipeline capability\n",
      1,
    ),
    (
      after(") "),
      established(),
      "svnserve: E210004: Malformed network data\n",
      1,
    ),
    (after("( get-lat"), established(), "", 0),
    (after(UPDATE_AND_PATH), reporting, CLOSED_ERR, 1),
    (after(COMMIT_AND_ROOT), editing, CLOSED_ERR, 1),
  ];
  let (ran, expected): (Vec<_>, Vec<_>) = rows
    .into_iter()
    .map(|(client, stock, err, status)| (scripted(&client).0, recorded(&stock, err, status)))
    .unzip();
  assert_eq!(ran, expected);
}

/// A-688 (the site's step 3: a service the tunnel's services do not take is the other services'). Do: through the site
/// whose other services serve nothing yet, ask git's upload-pack for a git repository, then `svnserve -t`. Expect: the
/// upload-pack refused as not served, in git's words; the tunnel run.
#[test]
fn the_tunnel_is_served_beside_the_other_services() {
  let (mut site, _) = site(NoKeys, principal(OWNER));
  let fetch = Asked::GitUploadPack {
    path: GIT_REPOSITORY.as_bytes().to_vec(),
  };
  let refused = match site.dispatch(&principal(OWNER), &fetch, None) {
    Dispatched::Refuse(refused) => Some(refused),
    Dispatched::Run(_) => None,
  };
  let ran = site.dispatch(&principal(OWNER), &Asked::Subversion, None);
  assert_eq!(
    refused,
    Some(Refused {
      stderr: b"fatal: git-upload-pack is not served yet\n".to_vec(),
      exit_status: GIT_DIE_STATUS,
    })
  );
  assert!(matches!(ran, Dispatched::Run(EitherCommand::Tunnel(_))));
}

/// Shape: the room the daemon's site is built with here: past anything the recorded session holds.
const DAEMON_ROOM: u64 = 1 << 20;

/// `bytes` with each `from` replaced by `to`.
fn replaced_all(bytes: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
  let mut out = Vec::new();
  let mut rest = bytes;
  while let Some(at) = rest.windows(from.len()).position(|window| window == from) {
    out.extend_from_slice(&rest[..at]);
    out.extend_from_slice(to);
    rest = &rest[at + from.len()..];
  }
  out.extend_from_slice(rest);
  out
}

/// `commit-info`'s 27-byte date in `bytes` (after `( 1 ( 27:`) replaced by `date`.
fn dated(bytes: &[u8], date: &[u8]) -> Vec<u8> {
  let mark = b"( 1 ( 27:";
  match bytes.windows(mark.len()).position(|window| window == mark) {
    Some(at) => {
      let start = at + mark.len();
      [
        &bytes[..start],
        date,
        &bytes[(start + 27).min(bytes.len())..],
      ]
      .concat()
    }
    None => bytes.to_vec(),
  }
}

/// The 36-byte UUID the realm names in a tunnel's output (`( EXTERNAL ) 36:`).
fn realm_of(bytes: &[u8]) -> Vec<u8> {
  let mark = b"( EXTERNAL ) 36:";
  bytes
    .windows(mark.len())
    .position(|window| window == mark)
    .and_then(|at| bytes.get(at + mark.len()..at + mark.len() + 36))
    .unwrap_or_default()
    .to_vec()
}

/// A-688 in the daemon (§4.17.4 piece 7; the tunnel's lender over A-662's map, A-698's step). Do: on a daemon's shard,
/// create the Subversion repository the recorded `mkdir` session names as its owner; then, from a task there, dispatch
/// `svnserve -t` through the daemon's SSH site (its services over its lenders) and hand it stock `svn` 1.15.0's
/// recorded `mkdir` connection. Expect: stock `svnserve -t`'s every byte, its greeting less `svndiff1`, the
/// repository's own UUID where the recording named its own, and `commit-info` naming the owner and the step's date:
/// revision 1; nothing on standard error and exit 0; every byte consumed; the repository's records at revision 1.
/// Before this the daemon's site refused `svnserve -t` as not served.
#[test]
fn the_daemons_ssh_site_serves_svnserve_t_and_lands_a_stock_commit() {
  let daemon = crate::daemon::audit_daemon();
  let id = crate::daemon::observe_first(&daemon, |state| {
    let created = crate::verbs::dispatch(
      state,
      1,
      &principal(OWNER),
      slates_ipc::protocol::RequestBody::CreateRepository {
        name: REPOSITORY.to_owned(),
        tool: slates_ipc::protocol::VcsTool::Subversion,
        format: slates_ipc::protocol::ObjectFormat::Sha1,
        default_branch: "trunk".to_owned(),
        green: None,
      },
    );
    assert!(
      !matches!(created, slates_ipc::protocol::ReplyBody::Refused { .. }),
      "{created:?}"
    );
    let record = state
      .db
      .partition()
      .repositories()
      .repository_named(REPOSITORY)
      .cloned();
    record.expect("the repository").id
  });
  let (sent, outcome) = std::sync::mpsc::sync_channel(1);
  crate::daemon::observe_first(&daemon, move |_state| {
    let task = slates_rt::futures::spawn(async move {
      let mut site = super::super::daemon_edge::daemon_site(DAEMON_ROOM);
      let _ = sent.send(tunnel(&mut site, &principal(OWNER), &[MKDIR_CLIENT]));
    })
    .expect("the tunnel's task");
    slates_rt::futures::detach(task).expect("detached");
  });
  let (answered, consumed) = outcome
    .recv_timeout(std::time::Duration::from_secs(60))
    .expect("the tunnel ended");
  let youngest = crate::daemon::observe_first(&daemon, move |state| {
    state
      .vcs_stores
      .subversion_records(id)
      .map(|records| records.youngest())
  });
  daemon.stop();
  let uuid = realm_of(&answered.out);
  let named = crate::lfs::owner_name(&principal(OWNER));
  let author = format!("( {}:{named} )", named.len());
  let stock = replaced(MKDIR_SERVER, RECORDED_AUTHOR, author.as_bytes());
  let mark = b"( 1 ( 27:";
  let date = answered
    .out
    .windows(mark.len())
    .position(|window| window == mark)
    .and_then(|at| answered.out.get(at + mark.len()..at + mark.len() + 27))
    .unwrap_or_default()
    .to_vec();
  let expected = dated(
    &replaced_all(&as_slates_writes(&stock), UUID.as_bytes(), &uuid),
    &date,
  );
  assert_eq!(
    (answered, consumed, youngest),
    (Answered::of(&expected, "", 0), MKDIR_CLIENT.len(), Some(1))
  );
}

/// A hook program written as a shell script into `dir`, named for its point, made executable and run once (macOS
/// assesses an executable at its first run, which is no part of the hook's): its path.
#[cfg(unix)]
#[allow(clippy::disallowed_methods)] // test code: its own scratch directory
fn hook_script(dir: &std::path::Path, point: &str, body: &str) -> String {
  use std::os::unix::fs::PermissionsExt;
  let path = dir.join(point);
  std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("the hook written");
  std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("made executable");
  let _ = std::process::Command::new(&path)
    .env_clear()
    .current_dir(dir)
    .output();
  path.to_string_lossy().into_owned()
}

/// A hook at `point` running `program`, named for the point.
#[cfg(unix)]
fn hook_at(
  point: slates_db::catalog::HookPointName,
  program: String,
) -> slates_db::catalog::HookConfig {
  slates_db::catalog::HookConfig {
    point,
    name: point.name().to_owned(),
    program,
  }
}

/// Runs `command` as [`drive`] does, each step awaited, a hook's run among them.
#[cfg(unix)]
async fn drive_awaiting<C: Command>(command: &mut C, runs: &[&[u8]]) -> Ended {
  let mut answered = Answered::default();
  let mut consumed = 0;
  let mut runs = runs.iter();
  let mut next = Next::More;
  for _ in 0..MOST_STEPS {
    take(command, &mut answered);
    match next {
      Next::Exited(status) => {
        answered.status = Some(status);
        return (answered, consumed);
      }
      Next::Input => hand_over(command, runs.next()),
      Next::More => {}
    }
    let stepped = command
      .step(ClientGone::never())
      .await
      .expect("a tunnel ends as svnserve ends, never failed");
    consumed += stepped.consumed;
    next = stepped.next;
  }
  panic!("a tunnel that never exits: {answered:?}");
}

/// The recorded `mkdir` connection's client bytes run through a tunnel whose repository's policy runs `hooks`, on a
/// runtime of its own (a hook is a process the run awaits): what the tunnel gave its client, and the youngest revision
/// then.
#[cfg(unix)]
fn commit_under_hooks(hooks: Vec<slates_db::catalog::HookConfig>) -> (Answered, u64) {
  use slates_rt::runtime::{LocalRuntime, RuntimeConfig};
  let lends = Lends::created();
  let held = Held {
    owner: principal(OWNER),
    hooks,
  };
  let services = SvnServices::new(NotYet, held.clone(), lends, BOUNDS);
  let mut site = ToolSite::new(NoKeys, held, services);
  let Dispatched::Run(mut command) = site.dispatch(&principal(OWNER), &Asked::Subversion, None)
  else {
    panic!("the tunnel was refused");
  };
  let runtime = LocalRuntime::new(&RuntimeConfig {
    shards: 1,
    tasks_per_shard: 8,
    timers_per_shard: 8,
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
  .expect("a local runtime");
  let (sent, got) = std::sync::mpsc::channel();
  runtime
    .spawn(async move {
      let bound = 10 * crate::daemon::LIVENESS_BUDGET_NS;
      let ran =
        slates_rt::futures::within(bound, drive_awaiting(&mut command, &[MKDIR_CLIENT])).await;
      let _ = sent.send(ran.ok().flatten());
    })
    .expect("the tunnel's task");
  runtime.run_until_idle();
  let (answered, _) = got
    .recv()
    .ok()
    .flatten()
    .expect("the tunnel ended within the test's bound");
  (answered, lends.youngest())
}

/// A-730 part 3 (`svnserve -t` runs its repository's hooks as `svnserve -d` does). Do: through a tunnel whose
/// repository's policy configures a start-commit hook that writes to standard error and exits 1, hand over the recorded
/// `mkdir` commit. Expect: the commit refused at `open-root` in `svnserve`'s words, "Commit blocked by start-commit hook
/// (exit code 1) with output:" and the hook's standard error (E165001); no revision made. Before A-730 the tunnel
/// refused a commit at a configured point in its own words, the hook never run.
#[cfg(unix)]
#[test]
fn a_tunnels_start_commit_hook_that_refuses_refuses_the_commit_in_svnserves_words() {
  let dir = crate::git_hooks::tests::ScratchDir::new();
  let refusing = hook_script(&dir.path, "start-commit", "echo 'not today' >&2\nexit 1");
  let (answered, youngest) = commit_under_hooks(vec![hook_at(
    slates_db::catalog::HookPointName::SvnStartCommit,
    refusing,
  )]);
  let out = String::from_utf8_lossy(&answered.out).into_owned();
  let words = "Commit blocked by start-commit hook (exit code 1) with output:\nnot today\n";
  assert!(
    out.contains("165001") && out.contains(&format!("{}:{words}", words.len())),
    "{out}"
  );
  assert_eq!(youngest, 0);
}

/// A-730 part 3. Do: through a tunnel whose repository's policy configures start-commit and pre-commit hooks that
/// record their arguments and accept, and a post-commit hook that writes to standard error and exits 1, hand over the
/// recorded `mkdir` commit. Expect: start-commit given the repository, the tunnel's user (the principal the key proved),
/// `mergeinfo` and the transaction; pre-commit the repository and the same transaction; the commit made as revision 1,
/// its `commit-info` carrying post-commit's failure in `svnserve`'s words.
#[cfg(unix)]
#[test]
fn a_tunnel_runs_its_repositorys_commit_hooks_as_svnserve_t_runs_them() {
  use slates_db::catalog::HookPointName;
  let dir = crate::git_hooks::tests::ScratchDir::new();
  let at = dir.path.display().to_string();
  let start = hook_script(
    &dir.path,
    "start-commit",
    &format!("for arg in \"$@\"; do echo \"$arg\"; done > {at}/start.args"),
  );
  let pre = hook_script(
    &dir.path,
    "pre-commit",
    &format!("for arg in \"$@\"; do echo \"$arg\"; done > {at}/pre.args"),
  );
  let post = hook_script(&dir.path, "post-commit", "echo 'post said no' >&2\nexit 1");
  let (answered, youngest) = commit_under_hooks(vec![
    hook_at(HookPointName::SvnStartCommit, start),
    hook_at(HookPointName::SvnPreCommit, pre),
    hook_at(HookPointName::SvnPostCommit, post),
  ]);
  #[allow(clippy::disallowed_methods)]
  // test code: what the hooks recorded in its own scratch directory
  let lines = |file: &str| -> Vec<String> {
    std::fs::read_to_string(dir.path.join(file))
      .unwrap_or_default()
      .lines()
      .map(str::to_owned)
      .collect()
  };
  let (started, precommitted) = (lines("start.args"), lines("pre.args"));
  let repos = format!("/{REPOSITORY}");
  let user = crate::lfs::owner_name(&principal(OWNER));
  assert_eq!(
    (
      started.first(),
      started.get(1),
      started.get(2).map(String::as_str),
      precommitted.first(),
      precommitted.get(1) == started.get(3),
    ),
    (
      Some(&repos),
      Some(&user),
      Some("mergeinfo"),
      Some(&repos),
      true
    ),
    "start-commit {started:?}, pre-commit {precommitted:?}"
  );
  let out = String::from_utf8_lossy(&answered.out).into_owned();
  assert!(out.contains("post said no"), "{out}");
  assert_eq!(youngest, 1);
}
