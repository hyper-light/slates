//! The commit's drive (A-617, §4.17.4 items 3.2 to 3.4, piece 7's commit): stock Subversion 1.15.0's own commits
//! (`crates/svn/fixtures/commit/`, recorded by its `make-fixtures.py`) replayed in their order through `begin`, the
//! drive and a commit, every answer compared byte for byte with what `svnserve` wrote; and each refusal of a hostile
//! drive in `svnserve`'s number and words, with the early-error rule.
use std::collections::BTreeMap;

use aws_lc_rs::digest::{SHA256, digest};

use super::*;
use crate::lock::{HeadView, LockRequest, LockTarget};
use crate::ra_svn::{Bounds, Parsed, Parser};

/// One item of a stream and the bytes it took.
type Read<'s> = (Value<'s>, &'s [u8]);

/// A recorded connection: its name, its principal and its two streams.
type Connection = (&'static str, &'static str, &'static [u8], &'static [u8]);

/// The repository's root URL in the recordings.
/// Format: `make-fixtures.py`'s `svn+fixture://fixture/repo`.
const REPOSITORY: &str = "svn+fixture://fixture/repo";

/// The longest word a stock parser reads.
/// Format: `marshal.c` `MAX_WORD_LENGTH` (66) less one.
const WORD_BYTES: usize = 24;

/// The deepest list a recorded stream holds, either way.
/// Shape: a `get-dir` answer's dirent list inside its entries inside the answer, inside the response, with room.
const DEPTH: usize = 8;

/// The room a test drive and the test's index allow: past anything the recordings hold.
const ROOM: usize = 1 << 30;

/// The commits, the lock and the race, in the order `make-fixtures.py` ran them (`sessions.txt`); the race is bob's
/// commit `svnserve` took while alice's `conflict` drive was held at `close-edit`.
const CONNECTIONS: [Connection; 17] = [
  ("mkdir", "alice", include_bytes!("../../../fixtures/commit/mkdir.1.client.bin"), include_bytes!("../../../fixtures/commit/mkdir.1.server.bin")),
  ("import", "alice", include_bytes!("../../../fixtures/commit/import.1.client.bin"), include_bytes!("../../../fixtures/commit/import.1.server.bin")),
  ("edit", "alice", include_bytes!("../../../fixtures/commit/edit.1.client.bin"), include_bytes!("../../../fixtures/commit/edit.1.server.bin")),
  ("edit-large", "alice", include_bytes!("../../../fixtures/commit/edit-large.1.client.bin"), include_bytes!("../../../fixtures/commit/edit-large.1.server.bin")),
  ("propset", "alice", include_bytes!("../../../fixtures/commit/propset.1.client.bin"), include_bytes!("../../../fixtures/commit/propset.1.server.bin")),
  ("propdel", "alice", include_bytes!("../../../fixtures/commit/propdel.1.client.bin"), include_bytes!("../../../fixtures/commit/propdel.1.server.bin")),
  ("root-prop-stale", "alice", include_bytes!("../../../fixtures/commit/root-prop-stale.1.client.bin"), include_bytes!("../../../fixtures/commit/root-prop-stale.1.server.bin")),
  ("root-prop", "alice", include_bytes!("../../../fixtures/commit/root-prop.1.client.bin"), include_bytes!("../../../fixtures/commit/root-prop.1.server.bin")),
  ("add-delete", "alice", include_bytes!("../../../fixtures/commit/add-delete.1.client.bin"), include_bytes!("../../../fixtures/commit/add-delete.1.server.bin")),
  ("replace", "alice", include_bytes!("../../../fixtures/commit/replace.1.client.bin"), include_bytes!("../../../fixtures/commit/replace.1.server.bin")),
  ("copy-url", "alice", include_bytes!("../../../fixtures/commit/copy-url.1.client.bin"), include_bytes!("../../../fixtures/commit/copy-url.1.server.bin")),
  ("copy-modify", "alice", include_bytes!("../../../fixtures/commit/copy-modify.1.client.bin"), include_bytes!("../../../fixtures/commit/copy-modify.1.server.bin")),
  ("out-of-date", "alice", include_bytes!("../../../fixtures/commit/out-of-date.1.client.bin"), include_bytes!("../../../fixtures/commit/out-of-date.1.server.bin")),
  ("lock-bob", "bob", include_bytes!("../../../fixtures/commit/lock-bob.1.client.bin"), include_bytes!("../../../fixtures/commit/lock-bob.1.server.bin")),
  ("locked-by-other", "alice", include_bytes!("../../../fixtures/commit/locked-by-other.1.client.bin"), include_bytes!("../../../fixtures/commit/locked-by-other.1.server.bin")),
  ("commit-with-lock", "bob", include_bytes!("../../../fixtures/commit/commit-with-lock.1.client.bin"), include_bytes!("../../../fixtures/commit/commit-with-lock.1.server.bin")),
  ("conflict", "alice", include_bytes!("../../../fixtures/commit/conflict.1.client.bin"), include_bytes!("../../../fixtures/commit/conflict.1.server.bin")),
];

/// Bob's commit that landed during alice's `conflict` drive.
const RACE: Connection = (
  "race-commit",
  "bob",
  include_bytes!("../../../fixtures/commit/race-commit.1.client.bin"),
  include_bytes!("../../../fixtures/commit/race-commit.1.server.bin"),
);

/// The file bob locks (`make-fixtures.py`'s `lock-bob` step, `svn lock -m bob's WORK/wc-bob/a.txt`).
const LOCKED: &str = "/trunk/project/a.txt";

/// A green over two maps, naming content by SHA-256 of a tagged encoding.
#[derive(Default)]
struct TestGreen {
  texts: BTreeMap<ContentIdentity, Vec<u8>>,
  lists: BTreeMap<ContentIdentity, Vec<(Vec<u8>, Vec<u8>)>>,
}

/// SHA-256 of `bytes`.
fn sha256(bytes: &[u8]) -> ContentIdentity {
  <[u8; 32]>::try_from(digest(&SHA256, bytes).as_ref()).unwrap()
}

impl Green for TestGreen {
  fn text(&self, identity: &ContentIdentity) -> Option<&[u8]> {
    self.texts.get(identity).map(Vec::as_slice)
  }

  fn properties(&self, identity: &ContentIdentity) -> Option<Vec<(&[u8], &[u8])>> {
    let list = self.lists.get(identity)?;
    Some(list.iter().map(|(name, value)| (name.as_slice(), value.as_slice())).collect())
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

/// The repository the replay builds: the index, the green and the lock table.
struct Replay {
  index: Index,
  green: TestGreen,
  locks: LockTable,
  transactions: u64,
}

impl Replay {
  /// An empty repository: revision 0, its root alone.
  fn new() -> Self {
    Self {
      index: Index::new(),
      green: TestGreen::default(),
      locks: LockTable::new(),
      transactions: 0,
    }
  }
}

/// Every item of a recorded stream.
fn items(stream: &[u8]) -> Vec<Read<'_>> {
  let bounds = Bounds {
    item_bytes: stream.len(),
    word_bytes: WORD_BYTES,
    depth: DEPTH,
    nodes: stream.len(),
  };
  let mut read = Vec::new();
  let mut rest = stream;
  while !rest.is_empty() {
    match Parser::new(bounds).parse(rest).unwrap() {
      Parsed::Item { value, consumed } => {
        read.push((value, &rest[..consumed]));
        rest = &rest[consumed..];
      }
      Parsed::NeedMore { needed } => panic!("a stream ends inside an item ({needed} bytes needed)"),
    }
  }
  read
}

/// Whether `item` is the command `name`.
fn is_command(item: &Value<'_>, name: &str) -> bool {
  matches!(item, Value::List(parts) if parts.first() == Some(&Value::Word(name)))
}

/// The session's path as the client last named it before item `before`: its greeting's URL or a `reparent`'s.
fn session_path(client: &[Read<'_>], before: usize) -> String {
  let mut url: &[u8] = b"";
  for (item, _) in &client[..before] {
    let Value::List(parts) = item else { continue };
    match parts.as_slice() {
      [Value::Number(_), Value::List(_), Value::String(named), ..] => url = named,
      [Value::Word("reparent"), Value::List(params)] => {
        if let Some(Value::String(named)) = params.first() {
          url = named;
        }
      }
      _ => {}
    }
  }
  let rest = std::str::from_utf8(url).unwrap().strip_prefix(REPOSITORY).unwrap();
  if rest.is_empty() { "/".to_owned() } else { rest.to_owned() }
}

/// The date of the revision a recorded commit answered, its last item's.
fn recorded_date(server: &[Read<'_>]) -> String {
  let Some((Value::List(parts), _)) = server.last() else { panic!("no commit-info") };
  let Some(Value::List(date)) = parts.get(1) else { panic!("no date") };
  let Some(Value::String(date)) = date.first() else { panic!("no date") };
  String::from_utf8(date.to_vec()).unwrap()
}

/// Commits an accepted work as the owner would and writes `svnserve`'s answer.
fn commit(replay: &mut Replay, work: Work, user: &str, date: &str, out: &mut Vec<u8>) {
  let revision = replay.index.append(work.record, ROOM).unwrap();
  replay.green.texts.extend(work.texts);
  replay.green.lists.extend(work.property_lists);
  for entry in &work.lock_entries {
    replay.locks.apply(entry).unwrap();
  }
  write_commit_info(revision, date, Some(user), None, out).unwrap();
}

/// Replays one recorded commit connection and returns what slates wrote from `commit` on; `race` lands just before
/// the drive's `close-edit`.
fn replay_commit(replay: &mut Replay, connection: Connection, race: Option<Connection>) -> Vec<u8> {
  let (name, user, client, server) = connection;
  let client = items(client);
  let at = client.iter().position(|(item, _)| is_command(item, "commit")).unwrap();
  let base_path = session_path(&client, at);
  let Value::List(parts) = &client[at].0 else { panic!("{name}: commit") };
  let Some(Value::List(params)) = parts.get(1) else { panic!("{name}: params") };
  let request = CommitRequest::read(params).unwrap();
  let mut out = Vec::new();
  let locks = begin(&request, &base_path, Some(user), &mut out).unwrap().unwrap();
  replay.transactions += 1;
  let setup = Setup {
    base_path,
    repository_url: REPOSITORY.to_owned(),
    filesystem: "fixture".to_owned(),
    transaction: format!("{}-{}", replay.index.youngest(), replay.transactions),
    locks,
    now: AprTime(0),
    room: ROOM,
  };
  let mut drive = Drive::new(setup);
  for (item, _) in &client[at + 1..] {
    if let Some(race) = race.filter(|_| is_command(item, "close-edit")) {
      let answered = replay_commit(replay, race, None);
      check_tail(race.0, race.3, &answered);
    }
    let step = drive.command(&replay.index, &replay.green, &replay.locks, item, &mut out).unwrap();
    if let Step::Accepted(work) = step {
      commit(replay, *work, user, &recorded_date(&items(server)), &mut out);
    }
  }
  out
}

/// Asserts that `answered` is what `svnserve` wrote last on the connection.
fn check_tail(name: &str, server: &[u8], answered: &[u8]) {
  let server = items(server);
  let count = items(answered).len();
  let tail: Vec<u8> = server[server.len() - count..].iter().flat_map(|(_, bytes)| bytes.iter().copied()).collect();
  assert_eq!(String::from_utf8_lossy(answered), String::from_utf8_lossy(&tail), "{name}");
}

/// Takes bob's lock as `svnserve` did, with the token it drew (A-602's guard over the token's UUID).
fn take_lock(replay: &mut Replay, server: &[u8]) {
  let token = items(server)
    .iter()
    .find_map(|(item, _)| find_token(item))
    .expect("the lock's answer carries its token");
  let uuid: Vec<u8> = token.strip_prefix("opaquelocktoken:").unwrap().bytes().filter(|byte| *byte != b'-').collect();
  let drawn: Vec<u8> = uuid
    .chunks(2)
    .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
    .collect();
  let mut draw = || drawn.clone().try_into().unwrap();
  let request = LockRequest {
    user: Some("bob".to_owned()),
    comment: Some(b"bob's".to_vec()),
    targets: BTreeMap::from([(LOCKED.to_owned(), LockTarget { current: None, token: None })]),
    ..LockRequest::default()
  };
  let decision = replay.locks.lock(&request, &Head(&replay.index), AprTime(0), ROOM, &mut draw).unwrap();
  for entry in &decision.entries {
    replay.locks.apply(entry).unwrap();
  }
  let held = replay.locks.get_lock(LOCKED, AprTime(0)).unwrap();
  assert_eq!(held.token.as_str(), token, "the drawn token is the recorded one");
}

/// The first lock token in `item`.
fn find_token(item: &Value<'_>) -> Option<String> {
  match item {
    Value::String(bytes) if bytes.starts_with(b"opaquelocktoken:") => String::from_utf8(bytes.to_vec()).ok(),
    Value::List(parts) => parts.iter().find_map(find_token),
    _ => None,
  }
}

/// The replay of every recorded connection up to (not including) `stop`.
fn replayed_until(stop: &str) -> Replay {
  let mut replay = Replay::new();
  for connection in CONNECTIONS {
    if connection.0 == stop {
      break;
    }
    replay_connection(&mut replay, connection);
  }
  replay
}

/// One recorded connection replayed and checked.
fn replay_connection(replay: &mut Replay, connection: Connection) {
  let (name, _, _, server) = connection;
  if name == "lock-bob" {
    take_lock(replay, server);
    return;
  }
  let race = (name == "conflict").then_some(RACE);
  let answered = replay_commit(replay, connection, race);
  check_tail(name, server, &answered);
}

/// A-617 (§4.17.4 items 3.2 to 3.4; `commit.c`, `editorp.c`, `serve.c` `commit`). Do: replay every recorded commit of
/// stock svn 1.15.0 against stock svnserve 1.15.0 in its order, bob's lock taken with its recorded token and bob's
/// race landing just before alice's `close-edit`, each accepted work committed to the index. Expect: every answer
/// from `commit` on is the bytes `svnserve` wrote: thirteen commits answered with their revisions 1 to 13, the race's
/// among them, the stale file and the stale root out of date (`E160028`), alice against bob's lock refused
/// (`E160039`) at the call that meets it, and alice's raced commit a conflict at bob's file (`E160024`); the index's
/// youngest is the recorded 13.
#[test]
fn stock_commits_are_answered_as_svnserve_answered_them() {
  let replay = replayed_until("");
  assert_eq!(replay.index.youngest(), 13);
}

/// A hostile drive: the commands after the request, what the drive writes, and how its last item ends.
struct Hostile {
  /// What the case does.
  name: &'static str,
  /// The editor commands, as a stock parser reads them.
  commands: &'static [u8],
  /// The failure's number and words, or `None` for no failure.
  failure: Option<(u32, &'static str)>,
  /// How the last command's step ends.
  last: Last,
}

/// How a hostile drive's last step ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Last {
  Taken,
  Refused,
  Dropped,
  Ended,
  Close,
}

/// The step's kind.
fn kind_of(step: &Step) -> Last {
  match step {
    Step::Taken | Step::Accepted(_) => Last::Taken,
    Step::Refused => Last::Refused,
    Step::Dropped => Last::Dropped,
    Step::Ended => Last::Ended,
    Step::Close => Last::Close,
  }
}

/// The hostile drives, each over the repository after `edit` (revision 3), the session at `/trunk/project`.
const HOSTILE: &[Hostile] = &[
  Hostile {
    name: "open-root past the youngest",
    commands: b"( open-root ( ( 9 ) 2:d0 ) ) ",
    failure: Some((160_006, "No such revision 9 (HEAD is 3)")),
    last: Last::Refused,
  },
  Hostile {
    name: "open-dir of an absent path",
    commands: b"( open-root ( ( 3 ) 2:d0 ) ) ( open-dir ( 4:nope 2:d0 2:d1 ( 3 ) ) ) ",
    failure: Some((160_016, "Path 'nope' not present")),
    last: Last::Refused,
  },
  Hostile {
    name: "delete-entry of an absent path",
    commands: b"( open-root ( ( 3 ) 2:d0 ) ) ( delete-entry ( 7:nowhere ( 3 ) 2:d0 ) ) ",
    failure: Some((160_028, "'/trunk/project/nowhere' is out of date")),
    last: Last::Refused,
  },
  Hostile {
    name: "add-file without a copy of a present path",
    commands: b"( open-root ( ( 3 ) 2:d0 ) ) ( add-file ( 5:a.txt 2:d0 2:c1 ( ) ) ) ",
    failure: Some((160_020, "File already exists: filesystem 'fixture', transaction '3-1', path '/trunk/project/a.txt'")),
    last: Last::Refused,
  },
  Hostile {
    name: "a copy from another repository",
    commands: b"( open-root ( ( 3 ) 2:d0 ) ) ( add-dir ( 1:b 2:d0 2:d1 ( 19:svn+other://x/trunk 1 ) ) ) ",
    failure: Some((160_000, "Source url 'svn+other://x/trunk' is from different repository")),
    last: Last::Refused,
  },
  Hostile {
    name: "close-edit before open-root",
    commands: b"( close-edit ( ) ) ",
    failure: Some((165_002, "No valid transaction supplied to close_edit")),
    last: Last::Refused,
  },
  Hostile {
    name: "an unknown command, then the rest dropped until abort-edit",
    commands: b"( rename-file ( ) ) ( open-root ( ( 3 ) 2:d0 ) ) ( abort-edit ( ) ) ",
    failure: Some((210_001, "Unknown editor command 'rename-file'")),
    last: Last::Ended,
  },
  Hostile {
    name: "a malformed item after a refusal is dropped too",
    commands: b"( close-edit ( ) ) ( 5 ) ",
    failure: Some((165_002, "No valid transaction supplied to close_edit")),
    last: Last::Dropped,
  },
  Hostile {
    name: "abort-edit while driving",
    commands: b"( open-root ( ( 3 ) 2:d0 ) ) ( abort-edit ( ) ) ",
    failure: None,
    last: Last::Ended,
  },
  Hostile {
    name: "a directory token never named",
    commands: b"( open-root ( ( 3 ) 2:d0 ) ) ( open-dir ( 3:sub 2:d9 2:d1 ( 3 ) ) ) ",
    failure: None,
    last: Last::Close,
  },
  Hostile {
    name: "a file token where a directory's is wanted",
    commands: b"( open-root ( ( 3 ) 2:d0 ) ) ( open-file ( 5:a.txt 2:d0 2:c1 ( 3 ) ) ) ( open-dir ( 3:sub 2:c1 2:d1 ( 3 ) ) ) ",
    failure: None,
    last: Last::Close,
  },
  Hostile {
    name: "a chunk with no text delta open",
    commands: b"( open-root ( ( 3 ) 2:d0 ) ) ( open-file ( 5:a.txt 2:d0 2:c1 ( 3 ) ) ) ( textdelta-chunk ( 2:c1 4:SVN\x00 ) ) ",
    failure: None,
    last: Last::Close,
  },
  Hostile {
    name: "a client's failure",
    commands: b"( open-root ( ( 3 ) 2:d0 ) ) ( failure ( ) ) ",
    failure: None,
    last: Last::Close,
  },
  Hostile {
    name: "a malformed command",
    commands: b"( open-root ( 2:d0 ) ) ",
    failure: None,
    last: Last::Close,
  },
  Hostile {
    name: "open-file of a stale file",
    commands: b"( open-root ( ( 3 ) 2:d0 ) ) ( open-file ( 5:a.txt 2:d0 2:c1 ( 2 ) ) ) ",
    failure: Some((160_028, "File '/trunk/project/a.txt' is out of date")),
    last: Last::Refused,
  },
  Hostile {
    name: "a base checksum that is not hexadecimal",
    commands: b"( open-root ( ( 3 ) 2:d0 ) ) ( open-file ( 5:a.txt 2:d0 2:c1 ( 3 ) ) ) ( apply-textdelta ( 2:c1 ( 32:zz000000000000000000000000000000 ) ) ) ",
    failure: Some((125_012, "Invalid character in hex checksum")),
    last: Last::Refused,
  },
  Hostile {
    name: "a delta that is not svndiff",
    commands: b"( open-root ( ( 3 ) 2:d0 ) ) ( open-file ( 5:a.txt 2:d0 2:c1 ( 3 ) ) ) ( apply-textdelta ( 2:c1 ( ) ) ) ( textdelta-chunk ( 2:c1 4:XYZ\x00 ) ) ",
    failure: Some((185_000, "Svndiff has invalid header")),
    last: Last::Refused,
  },
  Hostile {
    name: "a delta ended part way",
    commands: b"( open-root ( ( 3 ) 2:d0 ) ) ( open-file ( 5:a.txt 2:d0 2:c1 ( 3 ) ) ) ( apply-textdelta ( 2:c1 ( ) ) ) ( textdelta-chunk ( 2:c1 2:SV ) ) ( textdelta-end ( 2:c1 ) ) ",
    failure: Some((185_004, "Unexpected end of svndiff input")),
    last: Last::Refused,
  },
];

/// Runs one hostile drive over `replay` and checks it.
fn check_hostile(replay: &Replay, case: &Hostile) {
  let setup = Setup {
    base_path: "/trunk/project".to_owned(),
    repository_url: REPOSITORY.to_owned(),
    filesystem: "fixture".to_owned(),
    transaction: "3-1".to_owned(),
    locks: CommitLocks { user: Some("alice".to_owned()), ..CommitLocks::default() },
    now: AprTime(0),
    room: ROOM,
  };
  let mut drive = Drive::new(setup);
  let mut out = Vec::new();
  let mut last = Last::Taken;
  for (item, _) in items(case.commands) {
    last = kind_of(&drive.command(&replay.index, &replay.green, &replay.locks, &item, &mut out).unwrap());
  }
  let failure = items(&out).into_iter().find_map(|(item, _)| failure_of(&item));
  let expected = case.failure.map(|(code, words)| (u64::from(code), words.to_owned()));
  assert_eq!((failure, last), (expected, case.last), "{}", case.name);
}

/// The number and words of a failure item.
fn failure_of(item: &Value<'_>) -> Option<(u64, String)> {
  let Value::List(parts) = item else { return None };
  let [Value::Word("failure"), Value::List(errors)] = parts.as_slice() else { return None };
  let Some(Value::List(error)) = errors.first() else { return None };
  let [Value::Number(code), Value::String(words), ..] = error.as_slice() else { return None };
  Some((*code, String::from_utf8_lossy(words).into_owned()))
}

/// A-617 (`editorp.c` 477-516, 762-826, 1100-1208; `commit.c` 413-882). Do: drive the repository after `edit` with
/// each hostile drive. Expect: each refusal written once in `svnserve`'s number and words, the drive dropping what
/// follows a refusal (a malformed item too) until `abort-edit`, `abort-edit` answered while driving, and every token
/// fault, a client's failure and a malformed command ending the connection with nothing written.
#[test]
fn hostile_drives_are_refused_as_svnserve_refuses_them() {
  let replay = replayed_until("edit-large");
  for case in HOSTILE {
    check_hostile(&replay, case);
  }
}

/// A-617 (`commit.c` 679-694; `checksum.c` 688-694). Do: open `a.txt` after `edit`, send it a delta that leaves it
/// whole, then close it with an MD5 that is not its text's. Expect: `E200014` in `svn_checksum_mismatch_err`'s layout,
/// the expected digest the client's and the actual one the text's.
#[test]
fn a_resulting_text_whose_md5_is_not_the_clients_is_refused() {
  let replay = replayed_until("edit-large");
  let text = b"alpha\nBETA\ngamma\ndelta\n";
  let actual = displayed(&md5_of(text).unwrap());
  let commands = b"( open-root ( ( 3 ) 2:d0 ) ) ( open-file ( 5:a.txt 2:d0 2:c1 ( 3 ) ) ) \
( close-file ( 2:c1 ( 32:00000000000000000000000000000001 ) ) ) ";
  let mut drive = Drive::new(Setup {
    base_path: "/trunk/project".to_owned(),
    repository_url: REPOSITORY.to_owned(),
    filesystem: "fixture".to_owned(),
    transaction: "3-1".to_owned(),
    locks: CommitLocks::default(),
    now: AprTime(0),
    room: ROOM,
  });
  let mut out = Vec::new();
  for (item, _) in items(commands) {
    drive.command(&replay.index, &replay.green, &replay.locks, &item, &mut out).unwrap();
  }
  let words = format!(
    "Checksum mismatch for resulting fulltext\n(/trunk/project/a.txt):\n   expected:  00000000000000000000000000000001\n     actual:  {actual}\n"
  );
  let failure = items(&out).into_iter().find_map(|(item, _)| failure_of(&item));
  assert_eq!(failure, Some((200_014, words)));
}

/// A-617 (module documentation: the room). Do: open a delta on `a.txt` with a room of four bytes and send it eight.
/// Expect: `E210009`, the work past its room, before the delta is read.
#[test]
fn a_work_past_its_room_is_refused() {
  let replay = replayed_until("edit-large");
  let commands = b"( open-root ( ( 3 ) 2:d0 ) ) ( open-file ( 5:a.txt 2:d0 2:c1 ( 3 ) ) ) \
( apply-textdelta ( 2:c1 ( ) ) ) ( textdelta-chunk ( 2:c1 8:SVN\x00abcd ) ) ";
  let mut drive = Drive::new(Setup {
    base_path: "/trunk/project".to_owned(),
    repository_url: REPOSITORY.to_owned(),
    filesystem: "fixture".to_owned(),
    transaction: "3-1".to_owned(),
    locks: CommitLocks::default(),
    now: AprTime(0),
    room: 4,
  });
  let mut out = Vec::new();
  for (item, _) in items(commands) {
    drive.command(&replay.index, &replay.green, &replay.locks, &item, &mut out).unwrap();
  }
  let failure = items(&out).into_iter().find_map(|(item, _)| failure_of(&item));
  assert_eq!(
    failure,
    Some((210_009, "The client request size exceeds the configured limit".to_owned()))
  );
}
