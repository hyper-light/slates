//! The connection's commits against stock `svnserve` (A-656 with A-655; §4.17.4 items 3.2 to 3.4,
//! piece 7).
//!
//! The oracle is stock Subversion 1.15.0's own commits (`crates/svn/fixtures/commit/`, recorded
//! by its `make-fixtures.py` through `svnserve -t`, each file's SHA-256 in `SHA256SUMS`): each
//! commit connection's client bytes are handed to a connection from their first byte, an item at a
//! time as the connection asks for more, in the order the recorder ran them, the connection
//! tunnelled as the recording's user; each work the connection accepts is made as the owner makes
//! it (appended to the index, its content held, its lock entries applied), and what the connection
//! writes must be what `svnserve` wrote, its greeting less `svndiff1` and every item byte for byte.
//! Bob's lock is taken as `svnserve` took it, with its recorded token, the lock verbs not being
//! served yet; the read-only connections (the checkouts, the updates, `import.2`) are the reads'
//! and the reporter's suites'.

use std::collections::BTreeMap;

use aws_lc_rs::digest::{SHA256, digest};

use super::super::*;
use super::{LIMITS, PLENTY, SVNDIFF1};
use crate::index::{Action, ContentIdentity, NodeKind};
use crate::lock::{HeadView, LockRequest, LockTarget};
use crate::mergeinfo::Contents;
use crate::report::drive::green::FixtureGuards;
use crate::report::recorded::{command, items, value_of};

/// A recorded connection: its name, its tunnel's user and its two streams.
type Recording = (&'static str, &'static [u8], &'static [u8], &'static [u8]);

/// The recorded connection `$name`, tunnelled as `$user`, its two files included at build time.
macro_rules! recording {
  ($name:literal, $user:literal) => {
    (
      $name,
      $user,
      include_bytes!(concat!(
        "../../../fixtures/commit/",
        $name,
        ".1.client.bin"
      )),
      include_bytes!(concat!(
        "../../../fixtures/commit/",
        $name,
        ".1.server.bin"
      )),
    )
  };
}

/// The commit connections and bob's lock, in the order the recorder ran them (`sessions.txt`).
const RECORDINGS: [Recording; 17] = [
  recording!("mkdir", b"alice"),
  recording!("import", b"alice"),
  recording!("edit", b"alice"),
  recording!("edit-large", b"alice"),
  recording!("propset", b"alice"),
  recording!("propdel", b"alice"),
  recording!("root-prop-stale", b"alice"),
  recording!("root-prop", b"alice"),
  recording!("add-delete", b"alice"),
  recording!("replace", b"alice"),
  recording!("copy-url", b"alice"),
  recording!("copy-modify", b"alice"),
  recording!("out-of-date", b"alice"),
  recording!("lock-bob", b"bob"),
  recording!("locked-by-other", b"alice"),
  recording!("commit-with-lock", b"bob"),
  recording!("conflict", b"alice"),
];

/// Bob's commit that `svnserve` took while alice's `conflict` drive was held at `close-edit`.
const RACE: Recording = recording!("race-commit", b"bob");

/// The commit connections replayed: every recording but bob's lock, and bob's race.
const REPLAYED: usize = 17;

/// The youngest revision once every recorded commit is made (`sessions.txt`: thirteen commits).
const YOUNGEST: Revision = 13;

/// The repository's root URL in the recordings (`make-fixtures.py` `URL`).
const ROOT: &[u8] = b"svn+fixture://fixture/repo";

/// The repository's identity, which is also its realm (`make-fixtures.py` `UUID`).
const IDENTITY: &[u8] = b"00000000-0000-4000-8000-00000000a617";

/// The file bob locks (`make-fixtures.py`'s `lock-bob` step, `svn lock -m bob's WORK/wc-bob/a.txt`).
const LOCKED: &str = "/trunk/project/a.txt";

/// The prefix of a lock token (`lock.c`'s `opaquelocktoken:` URI).
const TOKEN_PREFIX: &str = "opaquelocktoken:";

/// The room a commit and the index allow: past anything the recordings hold.
const ROOM: usize = 1 << 30;

/// A green the replay builds from the works it makes: content by identity, every revision's
/// properties, and every revision's tree of kinds, from which a directory's entries are read.
#[derive(Debug)]
struct Built {
  /// The texts, by identity.
  texts: BTreeMap<ContentIdentity, Vec<u8>>,
  /// The property lists, by identity.
  lists: BTreeMap<ContentIdentity, Vec<(Vec<u8>, Vec<u8>)>>,
  /// Every revision's properties; revision 0's date was not recorded.
  revisions: Vec<Vec<(Vec<u8>, Vec<u8>)>>,
  /// Every revision's tree: each path's kind.
  trees: Vec<BTreeMap<String, NodeKind>>,
}

impl Built {
  /// A new repository's: revision 0, its root alone.
  fn new() -> Self {
    Self {
      texts: BTreeMap::new(),
      lists: BTreeMap::new(),
      revisions: vec![Vec::new()],
      trees: vec![BTreeMap::from([("/".to_owned(), NodeKind::Directory)])],
    }
  }

  /// The next revision, made of `work` at `date`: its rows applied in path order, a deletion or a
  /// replacement taking the path's subtree, an addition or a replacement by a copy bringing its
  /// source's.
  fn hold(&mut self, work: &Work, date: &str) {
    let mut tree = self.trees.last().unwrap().clone();
    let mut rows: Vec<_> = work.record.changes.iter().collect();
    rows.sort_by(|left, right| left.path.cmp(&right.path));
    for row in rows {
      if matches!(row.action, Action::Delete | Action::Replace) {
        tree.retain(|path, _| !at_or_beneath(path, &row.path));
      }
      if matches!(row.action, Action::Add | Action::Replace) {
        tree.insert(row.path.clone(), row.kind);
        if let Some(source) = &row.copy_from {
          let before = &self.trees[usize::try_from(source.revision).unwrap()];
          for (path, kind) in before {
            if path != &source.path && at_or_beneath(path, &source.path) {
              let rest = if source.path == "/" {
                path.as_str()
              } else {
                &path[source.path.len()..]
              };
              tree.insert(format!("{}{rest}", row.path), *kind);
            }
          }
        }
      }
    }
    self.trees.push(tree);
    self.texts.extend(work.texts.clone());
    self.lists.extend(work.property_lists.clone());
    let mut properties = work.revision_properties.clone();
    properties.push((b"svn:date".to_vec(), date.as_bytes().to_vec()));
    self.revisions.push(properties);
  }
}

/// Whether `path` is `place` or beneath it.
fn at_or_beneath(path: &str, place: &str) -> bool {
  path == place
    || place == "/"
    || path
      .strip_prefix(place)
      .is_some_and(|rest| rest.starts_with('/'))
}

/// SHA-256 of `bytes`, the replay's names for content.
fn sha256(bytes: &[u8]) -> ContentIdentity {
  digest(&SHA256, bytes).as_ref().try_into().unwrap()
}

impl Contents for Built {
  fn entries(&self, directory: &str, revision: Revision) -> Option<Vec<String>> {
    let tree = self.trees.get(usize::try_from(revision).ok()?)?;
    if *tree.get(directory)? != NodeKind::Directory {
      return None;
    }
    let prefix = if directory == "/" {
      "/".to_owned()
    } else {
      format!("{directory}/")
    };
    Some(
      tree
        .keys()
        .filter_map(|path| path.strip_prefix(&prefix))
        .filter(|name| !name.is_empty() && !name.contains('/'))
        .map(str::to_owned)
        .collect(),
    )
  }

  fn mergeinfo_value(&self, properties: &ContentIdentity) -> Option<&[u8]> {
    self
      .lists
      .get(properties)?
      .iter()
      .find(|(name, _)| name == b"svn:mergeinfo")
      .map(|(_, value)| value.as_slice())
  }
}

impl reads::Repository for Built {
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
    self
      .texts
      .get(identity)?
      .get(usize::try_from(offset).ok()?..)
  }
}

impl commit_drive::Green for Built {
  fn text(&self, identity: &ContentIdentity) -> Option<&[u8]> {
    self.texts.get(identity).map(Vec::as_slice)
  }

  fn properties(&self, identity: &ContentIdentity) -> Option<Vec<(&[u8], &[u8])>> {
    reads::Repository::properties(self, identity)
  }

  fn name_text(&self, text: &[u8]) -> ContentIdentity {
    sha256(&[b"text\0".as_slice(), text].concat())
  }

  fn name_properties(&self, properties: &[(Vec<u8>, Vec<u8>)]) -> ContentIdentity {
    let mut encoded = b"properties\0".to_vec();
    for (name, value) in properties {
      for part in [name, value] {
        encoded.extend_from_slice(&u64::try_from(part.len()).unwrap().to_be_bytes());
        encoded.extend_from_slice(part);
      }
    }
    sha256(&encoded)
  }
}

/// The head as the lock guard reads it.
struct Head<'i>(&'i Index);

impl HeadView for Head<'_> {
  fn youngest(&self) -> Revision {
    self.0.youngest()
  }

  fn node(&self, path: &str) -> Option<(NodeKind, Revision)> {
    let node = self.0.node(path, self.0.youngest()).ok()??;
    Some((node.kind(), node.created_revision()))
  }
}

/// The repository the replay builds: the index, the green and the lock table, and the
/// transactions begun.
struct Replay {
  index: Index,
  green: Built,
  locks: LockTable,
  transactions: u64,
}

impl Replay {
  /// An empty repository: revision 0, its root alone.
  fn new() -> Self {
    Self {
      index: Index::new(),
      green: Built::new(),
      locks: LockTable::new(),
      transactions: 0,
    }
  }

  /// What every call hands over, the guards `guards` (`make-fixtures.py`'s `svnserve.conf`: no
  /// anonymous access, authenticated users write, no authorization file).
  fn sources<'r>(&'r self, guards: &'r FixtureGuards) -> Sources<'r, Built, FixtureGuards> {
    Sources {
      index: &self.index,
      repository: &self.green,
      guards,
      locks: &self.locks,
      policy: Policy {
        anonymous: authz::Access::None,
        authenticated: authz::Access::Write,
        authz: None,
        filesystem: "fixture",
      },
    }
  }
}

/// The date `svnserve` answered a recorded commit with, its last item's; none for a refused one.
fn recorded_date(server: &[u8]) -> Option<String> {
  let last = *items(server).last()?;
  let Value::List(parts) = value_of(last) else {
    return None;
  };
  let Some(Value::List(date)) = parts.get(1) else {
    return None;
  };
  let Some(Value::String(date)) = date.first() else {
    return None;
  };
  Some(String::from_utf8(date.to_vec()).unwrap())
}

/// Replays one recorded connection through a connection tunnelled as its user, each accepted work
/// made as the owner makes it; `race` is replayed whole just before the client's `close-edit` is
/// handed over. Returns what the connection wrote.
fn replay_connection(replay: &mut Replay, recording: Recording, race: Option<Recording>) -> Vec<u8> {
  let (name, user, client, server) = recording;
  let tunnel = Repository {
    uuid: IDENTITY,
    root_url: ROOT,
    realm: IDENTITY,
    mergeinfo: true,
    anonymous: false,
    tunnel_user: Some(user),
    cram_md5: false,
  };
  let author = std::str::from_utf8(user).unwrap();
  let guards = FixtureGuards::default();
  let mut out = Vec::new();
  let mut connection = Connection::new(LIMITS, &mut out).unwrap();
  let mut next = items(client).into_iter();
  let mut held: Vec<u8> = Vec::new();
  let mut established = false;
  loop {
    let sources = replay.sources(&guards);
    let turn = connection.receive(established.then_some(&sources), &held, &mut out, PLENTY);
    match turn {
      Ok(Turn::NeedMore { .. }) => {
        let Some(item) = next.next() else {
          return out;
        };
        let closing = command(item).is_some_and(|(word, _)| word == "close-edit");
        if let Some(race) = race.filter(|_| closing) {
          let raced = replay_connection(replay, race, None);
          assert_as_svnserve(race.0, &raced, race.3);
        }
        held.extend_from_slice(item);
      }
      Ok(Turn::Took { consumed }) => drop(held.drain(..consumed)),
      Ok(Turn::Wrote) => {}
      Ok(Turn::Opened { consumed, .. }) => {
        held.drain(..consumed);
        assert_eq!(connection.open(&tunnel, &mut out), Ok(Access::Asked));
      }
      Ok(Turn::Established { consumed, .. }) => {
        held.drain(..consumed);
        established = true;
      }
      Ok(Turn::CommitAsked { consumed }) => {
        held.drain(..consumed);
        replay.transactions += 1;
        let start = CommitStart {
          transaction: format!("{}-{}", replay.index.youngest(), replay.transactions),
          now: AprTime(0),
          room: ROOM,
        };
        let sources = replay.sources(&guards);
        connection.commit(start, &sources, &mut out).unwrap();
      }
      Ok(Turn::Accepted { work, consumed }) => {
        held.drain(..consumed);
        let date = recorded_date(server).unwrap();
        replay.green.hold(&work, &date);
        let work = *work;
        for entry in &work.lock_entries {
          replay.locks.apply(entry).unwrap();
        }
        let revision = replay.index.append(work.record, ROOM).unwrap();
        let made = Made {
          revision,
          date: &date,
          author: Some(author),
          post_commit_error: None,
        };
        connection.committed(&made, &mut out).unwrap();
      }
      other => panic!("{name}: {other:?}"),
    }
  }
}

/// Asserts that `ours` is what `svnserve` wrote on the connection `name`: its greeting less
/// `svndiff1`, then every item byte for byte.
fn assert_as_svnserve(name: &str, ours: &[u8], server: &[u8]) {
  let (ours, theirs) = (items(ours), items(server));
  let greeting = theirs[0];
  let at = greeting
    .windows(SVNDIFF1.len())
    .position(|window| window == SVNDIFF1)
    .unwrap();
  let greeting = [&greeting[..at], &greeting[at + SVNDIFF1.len()..]].concat();
  assert_eq!(ours[0], greeting.as_slice(), "{name}: the greeting");
  assert_eq!(ours.len(), theirs.len(), "{name}: the items");
  for (index, (ours, theirs)) in ours.iter().zip(&theirs).enumerate().skip(1) {
    assert_eq!(
      String::from_utf8_lossy(ours),
      String::from_utf8_lossy(theirs),
      "{name}: item {index}"
    );
  }
}

/// Takes bob's lock as `svnserve` did, with the token it drew (A-602's guard over the token's
/// UUID).
fn take_lock(replay: &mut Replay, server: &[u8]) {
  let token = items(server)
    .into_iter()
    .find_map(|item| find_token(&value_of(item)))
    .unwrap();
  let uuid: Vec<u8> = token
    .strip_prefix(TOKEN_PREFIX)
    .unwrap()
    .bytes()
    .filter(|byte| *byte != b'-')
    .collect();
  let drawn: Vec<u8> = uuid
    .chunks(2)
    .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
    .collect();
  let mut draw = || drawn.clone().try_into().unwrap();
  let request = LockRequest {
    user: Some("bob".to_owned()),
    comment: Some(b"bob's".to_vec()),
    targets: BTreeMap::from([(
      LOCKED.to_owned(),
      LockTarget {
        current: None,
        token: None,
      },
    )]),
    ..LockRequest::default()
  };
  let decision = replay
    .locks
    .lock(&request, &Head(&replay.index), AprTime(0), ROOM, &mut draw)
    .unwrap();
  for entry in &decision.entries {
    replay.locks.apply(entry).unwrap();
  }
  let held = replay.locks.get_lock(LOCKED, AprTime(0)).unwrap();
  assert_eq!(held.token.as_str(), token, "the recorded token");
}

/// The first lock token in `value`.
fn find_token(value: &Value<'_>) -> Option<String> {
  match value {
    Value::String(bytes) if bytes.starts_with(TOKEN_PREFIX.as_bytes()) => {
      String::from_utf8(bytes.to_vec()).ok()
    }
    Value::List(parts) => parts.iter().find_map(find_token),
    _ => None,
  }
}

/// A-656 with A-655 (`serve.c` `commit` over A-655's drive through `svnserve -t`; stock `svnserve`
/// 1.15.0's recorded commits): replay every recorded commit connection from its first byte through
/// a connection tunnelled as its user (`EXTERNAL`), in the recorder's order, bob's lock taken as
/// `svnserve` took it and bob's race replayed just before alice's `close-edit`, each accepted work
/// made as the owner makes it; expect every connection to write what `svnserve` wrote, its
/// greeting less `svndiff1` and every item byte for byte: thirteen commits answered with their
/// revisions, the stale root and the stale file refused out of date (`E160028`), alice refused
/// against bob's lock (`E160039`) and her raced commit a conflict at bob's file (`E160024`), each
/// failure where the drive met it; and the youngest revision the recorded 13.
#[test]
fn every_recorded_commit_is_answered_as_svnserve_answered_it() {
  let mut replay = Replay::new();
  let mut replayed = 0;
  for recording in RECORDINGS {
    if recording.0 == "lock-bob" {
      take_lock(&mut replay, recording.3);
      continue;
    }
    let race = (recording.0 == "conflict").then_some(RACE);
    let out = replay_connection(&mut replay, recording, race);
    assert_as_svnserve(recording.0, &out, recording.3);
    replayed += 1 + usize::from(race.is_some());
  }
  assert_eq!(replayed, REPLAYED);
  assert_eq!(replay.index.youngest(), YOUNGEST);
}
