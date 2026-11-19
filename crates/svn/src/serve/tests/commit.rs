//! The connection's commits against stock `svnserve` (A-656 with A-655; §4.17.4 items 3.2 to 3.4,
//! piece 7).
//!
//! The oracle is stock Subversion 1.15.0's own commits (`crates/svn/fixtures/commit/`, recorded
//! by its `make-fixtures.py` through `svnserve -t`, each file's SHA-256 in `SHA256SUMS`): each
//! commit connection's client bytes are handed to a connection from their first byte, an item at a
//! time as the connection asks for more, in the order the recorder ran them, the connection
//! tunnelled as the recording's user; every hook the drive waits for passes, as the recorder's
//! repository has none; each work the connection accepts is committed through A-655's step
//! (`Revision::of_work` at the instant `svnserve` recorded, then the owner's records' `commit`),
//! and what the connection writes must be what `svnserve` wrote, its greeting less `svndiff1` and
//! every item byte for byte, but for a property list `svnserve` wrote in its hash table's order
//! (`get_props` fills the hash, `serve.c` 1089-1118 at `d434dc14`, and `svn_ra_svn__write_proplist`
//! walks it, `marshal.c` 799-814): those are compared as sets, and counted.
//! Bob's lock is taken as `svnserve` took it, with its recorded token, the lock verbs not being
//! served yet; the read-only connections (the checkouts, the updates, `import.2`) are the reads'
//! and the reporter's suites'.

use std::collections::BTreeMap;

use super::super::*;
use super::{LIMITS, PLENTY, SVNDIFF1};
use crate::commit::revision::Revision as RevisionToCommit;
use crate::dav::request::svn_time;
use crate::dump::Uuid;
use crate::index::{Action, ContentIdentity, NodeKind};
use crate::lock::{HeadView, LockRequest, LockTarget};
use crate::mergeinfo::Contents;
use crate::migration::{Repository as Records, Room};
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
      include_bytes!(concat!("../../../fixtures/commit/", $name, ".1.client.bin")),
      include_bytes!(concat!("../../../fixtures/commit/", $name, ".1.server.bin")),
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

/// The items `svnserve` wrote with a property list in another order than slates' sorted one, its
/// hash table's order (`get_props`, `serve.c` 1089-1118): compared as sets, and counted so the
/// count cannot grow unseen. One: the entry properties of `import`'s `get-dir` before its commit.
/// Shape: measured on Windows x86_64 (jupiter), 2026-11-19.
const IN_HASH_ORDER: usize = 1;

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

/// The bytes the owner's records may hold: past anything the recordings hold.
const ROOM_BYTES: u64 = 1 << 30;

/// Revision 0's `svn:date`, which no recording shows.
const CREATED: &[u8] = b"2026-11-19T05:05:00.000000Z";

/// What the reads of the replay's repository read, built from the works it commits: content by
/// identity, every revision's properties, and every revision's tree of kinds, from which a
/// directory's entries are read (the owner's records keep no entries).
#[derive(Debug)]
struct Built {
  /// The texts, by identity.
  texts: BTreeMap<ContentIdentity, Vec<u8>>,
  /// The property lists, by identity.
  lists: BTreeMap<ContentIdentity, Vec<(Vec<u8>, Vec<u8>)>>,
  /// Every revision's properties.
  revisions: Vec<Vec<(Vec<u8>, Vec<u8>)>>,
  /// Every revision's tree: each path's kind.
  trees: Vec<BTreeMap<String, NodeKind>>,
}

impl Built {
  /// A new repository's: revision 0, its root alone, dated [`CREATED`].
  fn new() -> Self {
    Self {
      texts: BTreeMap::new(),
      lists: BTreeMap::new(),
      revisions: vec![vec![(b"svn:date".to_vec(), CREATED.to_vec())]],
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

/// One repository as a connection reads it: the reads from the replay's [`Built`], the drive from
/// the owner's records, which name the content the drive makes; one index, the records'.
struct View<'r> {
  records: &'r Records,
  built: &'r Built,
}

impl Contents for View<'_> {
  fn entries(&self, directory: &str, revision: Revision) -> Option<Vec<String>> {
    self.built.entries(directory, revision)
  }

  fn mergeinfo_value(&self, properties: &ContentIdentity) -> Option<&[u8]> {
    self.built.mergeinfo_value(properties)
  }
}

impl reads::Repository for View<'_> {
  fn revision_properties(&self, revision: Revision) -> Option<Vec<(&[u8], &[u8])>> {
    reads::Repository::revision_properties(self.built, revision)
  }

  fn properties(&self, identity: &ContentIdentity) -> Option<Vec<(&[u8], &[u8])>> {
    reads::Repository::properties(self.built, identity)
  }

  fn text(&self, identity: &ContentIdentity, offset: u64) -> Option<&[u8]> {
    reads::Repository::text(self.built, identity, offset)
  }
}

impl commit_drive::Green for View<'_> {
  fn text(&self, identity: &ContentIdentity) -> Option<&[u8]> {
    commit_drive::Green::text(self.records, identity)
  }

  fn properties(&self, identity: &ContentIdentity) -> Option<Vec<(&[u8], &[u8])>> {
    commit_drive::Green::properties(self.records, identity)
  }

  fn name_text(&self, text: &[u8]) -> ContentIdentity {
    commit_drive::Green::name_text(self.records, text)
  }

  fn name_properties(&self, properties: &[(Vec<u8>, Vec<u8>)]) -> ContentIdentity {
    commit_drive::Green::name_properties(self.records, properties)
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

/// The repository the replay builds: the owner's records (index, contents, lock table), the reads'
/// view of them, and the transactions begun.
struct Replay {
  records: Records,
  built: Built,
  transactions: u64,
  /// The items that matched `svnserve`'s only with their property lists as sets.
  in_hash_order: usize,
}

impl Replay {
  /// An empty repository: revision 0, its root alone.
  fn new() -> Self {
    let room = Room {
      entries: ROOM,
      bytes: ROOM_BYTES,
    };
    let uuid = Uuid::parse(IDENTITY).unwrap();
    Self {
      records: Records::new(uuid, CREATED, room).unwrap(),
      built: Built::new(),
      transactions: 0,
      in_hash_order: 0,
    }
  }

  /// The repository as a connection reads it.
  fn view(&self) -> View<'_> {
    View {
      records: &self.records,
      built: &self.built,
    }
  }
}

/// What every call hands over, the guards `guards` (`make-fixtures.py`'s `svnserve.conf`: no
/// anonymous access, authenticated users write, no authorization file).
fn sources<'s>(
  view: &'s View<'s>,
  guards: &'s FixtureGuards,
) -> Sources<'s, View<'s>, FixtureGuards> {
  Sources {
    index: view.records.index(),
    repository: view,
    guards,
    locks: view.records.locks(),
    policy: Policy {
      anonymous: authz::Access::None,
      authenticated: authz::Access::Write,
      authz: None,
      filesystem: "fixture",
    },
  }
}

/// Commits an accepted `work` through A-655's step at the instant `svnserve` recorded for it, and
/// answers it on `connection`.
fn make(
  replay: &mut Replay,
  connection: &mut Connection,
  work: Work,
  recorded: &str,
  out: &mut Vec<u8>,
) {
  let now = svn_time(recorded).unwrap();
  replay.built.hold(&work, recorded);
  let revision = RevisionToCommit::of_work(work, now).unwrap();
  let date = revision.date().unwrap().to_owned();
  let author = revision.author().map(str::to_owned);
  let number = replay.records.commit(revision).unwrap();
  assert_eq!(
    date, recorded,
    "svn:date the owner's instant, as svnserve wrote it"
  );
  let made = Made {
    revision: number,
    date: &date,
    author: author.as_deref(),
    post_commit_error: None,
  };
  connection.committed(&made, out).unwrap();
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

/// Replays one recorded connection through a connection tunnelled as its user, every hook
/// passing and each accepted work committed through A-655's step; `race` is replayed whole just
/// before the client's `close-edit` is handed over. Returns what the connection wrote.
fn replay_connection(
  replay: &mut Replay,
  recording: Recording,
  race: Option<Recording>,
) -> Vec<u8> {
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
  let guards = FixtureGuards::default();
  let mut out = Vec::new();
  let mut connection = Connection::new(LIMITS, &mut out).unwrap();
  let mut next = items(client).into_iter();
  let mut held: Vec<u8> = Vec::new();
  let mut established = false;
  loop {
    let view = replay.view();
    let given = sources(&view, &guards);
    let turn = connection.receive(established.then_some(&given), &held, &mut out, PLENTY);
    match turn {
      Ok(Turn::NeedMore { .. }) => {
        let Some(item) = next.next() else {
          return out;
        };
        let closing = command(item).is_some_and(|(word, _)| word == "close-edit");
        if let Some(race) = race.filter(|_| closing) {
          let raced = replay_connection(replay, race, None);
          replay.in_hash_order += assert_as_svnserve(race.0, &raced, race.3);
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
          transaction: format!("{}-{}", replay.records.youngest(), replay.transactions),
          now: AprTime(0),
          room: ROOM,
        };
        let view = replay.view();
        connection
          .commit(start, &sources(&view, &guards), &mut out)
          .unwrap();
      }
      Ok(Turn::Hook { consumed, .. }) => {
        held.drain(..consumed);
        let view = replay.view();
        let answered = connection.answer_hook(Ok(()), &sources(&view, &guards), &mut out);
        match answered {
          Ok(Turn::Took { consumed: 0 }) => {}
          Ok(Turn::Accepted { work, consumed: 0 }) => {
            let recorded = recorded_date(server).unwrap();
            make(replay, &mut connection, *work, &recorded, &mut out);
          }
          other => panic!("{name}: the hook's answer: {other:?}"),
        }
      }
      Ok(Turn::Accepted { work, consumed }) => {
        held.drain(..consumed);
        let recorded = recorded_date(server).unwrap();
        make(replay, &mut connection, *work, &recorded, &mut out);
      }
      other => panic!("{name}: {other:?}"),
    }
  }
}

/// Asserts that `ours` is what `svnserve` wrote on the connection `name`: its greeting less
/// `svndiff1`, then every item byte for byte, or the same tree with its property lists as sets
/// where `svnserve` wrote one in hash order; answers how many items matched only so.
fn assert_as_svnserve(name: &str, ours: &[u8], server: &[u8]) -> usize {
  let (ours, theirs) = (items(ours), items(server));
  let greeting = theirs[0];
  let at = greeting
    .windows(SVNDIFF1.len())
    .position(|window| window == SVNDIFF1)
    .unwrap();
  let greeting = [&greeting[..at], &greeting[at + SVNDIFF1.len()..]].concat();
  assert_eq!(ours[0], greeting.as_slice(), "{name}: the greeting");
  assert_eq!(ours.len(), theirs.len(), "{name}: the items");
  let mut in_hash_order = 0;
  for (index, (ours, theirs)) in ours.iter().zip(&theirs).enumerate().skip(1) {
    if ours != theirs {
      assert_eq!(
        super::tree(&value_of(ours)),
        super::tree(&value_of(theirs)),
        "{name}: item {index}: {} against {}",
        String::from_utf8_lossy(ours),
        String::from_utf8_lossy(theirs)
      );
      in_hash_order += 1;
    }
  }
  in_hash_order
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
    .records
    .locks()
    .lock(
      &request,
      &Head(replay.records.index()),
      AprTime(0),
      ROOM,
      &mut draw,
    )
    .unwrap();
  replay
    .records
    .apply_lock_entries(&decision.entries)
    .unwrap();
  let held = replay.records.locks().get_lock(LOCKED, AprTime(0)).unwrap();
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
/// made as the owner makes it; expect every connection to write what `svnserve` wrote, its greeting
/// less `svndiff1` and every item byte for byte (the property lists `svnserve` wrote in hash order
/// as sets, counted): thirteen commits answered with their revisions, the stale root and the stale
/// file refused out of date (`E160028`), alice refused against bob's lock (`E160039`) and her raced
/// commit a conflict at bob's file (`E160024`), each failure where the drive met it; and the
/// youngest revision the recorded 13.
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
    replay.in_hash_order += assert_as_svnserve(recording.0, &out, recording.3);
    replayed += 1 + usize::from(race.is_some());
  }
  assert_eq!(replayed, REPLAYED);
  assert_eq!(
    replay.in_hash_order, IN_HASH_ORDER,
    "items matched only as sets"
  );
  assert_eq!(replay.records.youngest(), YOUNGEST);
}
