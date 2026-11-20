//! The commit's drive (A-617, §4.17.4 items 3.2 to 3.4, piece 7's commit): stock Subversion 1.15.0's own commits
//! (`crates/svn/fixtures/commit/`, recorded by its `make-fixtures.py`) replayed in their order through `begin`, the
//! drive, its hooks (A-661) and the commit step (A-655), every answer compared byte for byte with what `svnserve`
//! wrote; and each refusal of a hostile drive in `svnserve`'s number and words, with the early-error rule.
use std::collections::BTreeMap;

use super::*;
use crate::authz;
use crate::commit::revision::{Revision as RevisionToCommit, StepRefusal};
use crate::dump::Uuid;
use crate::lock::{HeadView, Lock, LockRequest, LockTarget};
use crate::migration::{Repository, Room};
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

/// The room the replay's repository holds its records within: past anything the recordings hold.
/// Shape: [`ROOM`] index entries and as many bytes.
const RECORDS_ROOM: Room = Room {
  entries: ROOM,
  bytes: 1 << 30,
};

/// The UUID the replay's repository is made with; no answer of a commit carries it.
const UUID: &[u8] = b"00000000-0000-4000-8000-000000000655";

/// The date the replay's repository is made at, its revision 0's.
const CREATED: &[u8] = b"2026-11-19T00:00:00.000000Z";

/// The commits, the lock and the race, in the order `make-fixtures.py` ran them (`sessions.txt`); the race is bob's
/// commit `svnserve` took while alice's `conflict` drive was held at `close-edit`.
const CONNECTIONS: [Connection; 17] = [
  (
    "mkdir",
    "alice",
    include_bytes!("../../../fixtures/commit/mkdir.1.client.bin"),
    include_bytes!("../../../fixtures/commit/mkdir.1.server.bin"),
  ),
  (
    "import",
    "alice",
    include_bytes!("../../../fixtures/commit/import.1.client.bin"),
    include_bytes!("../../../fixtures/commit/import.1.server.bin"),
  ),
  (
    "edit",
    "alice",
    include_bytes!("../../../fixtures/commit/edit.1.client.bin"),
    include_bytes!("../../../fixtures/commit/edit.1.server.bin"),
  ),
  (
    "edit-large",
    "alice",
    include_bytes!("../../../fixtures/commit/edit-large.1.client.bin"),
    include_bytes!("../../../fixtures/commit/edit-large.1.server.bin"),
  ),
  (
    "propset",
    "alice",
    include_bytes!("../../../fixtures/commit/propset.1.client.bin"),
    include_bytes!("../../../fixtures/commit/propset.1.server.bin"),
  ),
  (
    "propdel",
    "alice",
    include_bytes!("../../../fixtures/commit/propdel.1.client.bin"),
    include_bytes!("../../../fixtures/commit/propdel.1.server.bin"),
  ),
  (
    "root-prop-stale",
    "alice",
    include_bytes!("../../../fixtures/commit/root-prop-stale.1.client.bin"),
    include_bytes!("../../../fixtures/commit/root-prop-stale.1.server.bin"),
  ),
  (
    "root-prop",
    "alice",
    include_bytes!("../../../fixtures/commit/root-prop.1.client.bin"),
    include_bytes!("../../../fixtures/commit/root-prop.1.server.bin"),
  ),
  (
    "add-delete",
    "alice",
    include_bytes!("../../../fixtures/commit/add-delete.1.client.bin"),
    include_bytes!("../../../fixtures/commit/add-delete.1.server.bin"),
  ),
  (
    "replace",
    "alice",
    include_bytes!("../../../fixtures/commit/replace.1.client.bin"),
    include_bytes!("../../../fixtures/commit/replace.1.server.bin"),
  ),
  (
    "copy-url",
    "alice",
    include_bytes!("../../../fixtures/commit/copy-url.1.client.bin"),
    include_bytes!("../../../fixtures/commit/copy-url.1.server.bin"),
  ),
  (
    "copy-modify",
    "alice",
    include_bytes!("../../../fixtures/commit/copy-modify.1.client.bin"),
    include_bytes!("../../../fixtures/commit/copy-modify.1.server.bin"),
  ),
  (
    "out-of-date",
    "alice",
    include_bytes!("../../../fixtures/commit/out-of-date.1.client.bin"),
    include_bytes!("../../../fixtures/commit/out-of-date.1.server.bin"),
  ),
  (
    "lock-bob",
    "bob",
    include_bytes!("../../../fixtures/commit/lock-bob.1.client.bin"),
    include_bytes!("../../../fixtures/commit/lock-bob.1.server.bin"),
  ),
  (
    "locked-by-other",
    "alice",
    include_bytes!("../../../fixtures/commit/locked-by-other.1.client.bin"),
    include_bytes!("../../../fixtures/commit/locked-by-other.1.server.bin"),
  ),
  (
    "commit-with-lock",
    "bob",
    include_bytes!("../../../fixtures/commit/commit-with-lock.1.client.bin"),
    include_bytes!("../../../fixtures/commit/commit-with-lock.1.server.bin"),
  ),
  (
    "conflict",
    "alice",
    include_bytes!("../../../fixtures/commit/conflict.1.client.bin"),
    include_bytes!("../../../fixtures/commit/conflict.1.server.bin"),
  ),
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

/// The repository the replay builds, whose records (the index, the texts and property lists, the revision
/// properties and the locks) the drive reads as its index, green and lock table, and the commit step commits into;
/// and the date `svnserve` gave each revision.
struct Replay {
  repository: Repository,
  transactions: u64,
  dates: BTreeMap<u64, String>,
  /// Where its drives' new texts go (`commit::drive::TextSink`).
  texts: crate::commit::drive::HeldTexts,
}

impl Replay {
  /// An empty repository: revision 0, its root alone.
  fn new() -> Self {
    let uuid = Uuid::parse(UUID).unwrap();
    Self {
      repository: Repository::new(uuid, CREATED, RECORDS_ROOM).unwrap(),
      transactions: 0,
      dates: BTreeMap::new(),
      texts: crate::commit::drive::HeldTexts::new(),
    }
  }

  /// The drive's step for `item` over the replay's records, every hook due passed at once.
  fn command(&self, drive: &mut Drive, item: &Value<'_>, out: &mut Vec<u8>) -> Step {
    let records = &self.repository;
    let step = drive
      .command(
        records.index(),
        records,
        records.locks(),
        &self.texts,
        item,
        out,
      )
      .unwrap();
    match step {
      Step::Hook(_) => self.resume(drive, Ok(()), out),
      other => other,
    }
  }

  /// The drive's step once its hook answered `answer`.
  fn resume(
    &self,
    drive: &mut Drive,
    answer: Result<(), Vec<(u32, String)>>,
    out: &mut Vec<u8>,
  ) -> Step {
    let records = &self.repository;
    drive
      .resume(
        records.index(),
        records,
        records.locks(),
        &self.texts,
        answer,
        out,
      )
      .unwrap()
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
  let rest = std::str::from_utf8(url)
    .unwrap()
    .strip_prefix(REPOSITORY)
    .unwrap();
  if rest.is_empty() {
    "/".to_owned()
  } else {
    rest.to_owned()
  }
}

/// The date of the revision a recorded commit answered, its last item's.
fn recorded_date(server: &[Read<'_>]) -> String {
  let Some((Value::List(parts), _)) = server.last() else {
    panic!("no commit-info")
  };
  let Some(Value::List(date)) = parts.get(1) else {
    panic!("no date")
  };
  let Some(Value::String(date)) = date.first() else {
    panic!("no date")
  };
  String::from_utf8(date.to_vec()).unwrap()
}

/// Commits an accepted work through the commit step at the instant `svnserve` dated it, and writes `svnserve`'s
/// answer from what the revision records: its number, its date and its author. Its record (A-655 part 2b) reads back
/// as the revision whole.
fn commit_work(replay: &mut Replay, work: Work, date: &str, out: &mut Vec<u8>) {
  let now = crate::dav::request::svn_time(date).unwrap();
  let revision = RevisionToCommit::of_work(work, now, &replay.texts).unwrap();
  let record = crate::commit::record::Record::Revision(revision.clone());
  assert_eq!(
    crate::commit::record::Record::decode(&record.encode()),
    Ok(record)
  );
  let dated = revision.date().unwrap().to_owned();
  let author = revision.author().map(str::to_owned);
  let number = replay.repository.commit(revision).unwrap();
  replay.dates.insert(number, date.to_owned());
  write_commit_info(number, &dated, author.as_deref(), None, out).unwrap();
}

/// Replays one recorded commit connection and returns what slates wrote from `commit` on, each hook passed as it
/// comes due; `race` lands while the drive's `close-edit` waits for pre-commit, before its verdict.
fn replay_commit(replay: &mut Replay, connection: Connection, race: Option<Connection>) -> Vec<u8> {
  let (name, user, client, server) = connection;
  let client = items(client);
  let at = client
    .iter()
    .position(|(item, _)| is_command(item, "commit"))
    .unwrap();
  let base_path = session_path(&client, at);
  let Value::List(parts) = &client[at].0 else {
    panic!("{name}: commit")
  };
  let Some(Value::List(params)) = parts.get(1) else {
    panic!("{name}: params")
  };
  let mut out = Vec::new();
  replay.transactions += 1;
  let start = Start {
    transaction: format!("{}-{}", replay.repository.youngest(), replay.transactions),
    now: AprTime(0),
    room: ROOM,
    authenticated_now: false,
  };
  let session = recorded_session(&base_path, Some(user), None);
  let begun = commit(params, &session, start, &mut out).unwrap();
  let Begun::Driving(mut drive) = begun else {
    panic!("{name}: {begun:?}")
  };
  for (item, _) in &client[at + 1..] {
    let records = &replay.repository;
    let mut step = drive
      .command(
        records.index(),
        records,
        records.locks(),
        &replay.texts,
        item,
        &mut out,
      )
      .unwrap();
    if let Step::Hook(due) = step {
      if let (Due::PreCommit { .. }, Some(race)) = (due, race) {
        let answered = replay_commit(replay, race, None);
        check_tail(race.0, race.3, &answered);
      }
      step = replay.resume(&mut drive, Ok(()), &mut out);
    }
    if let Step::Accepted(work) = step {
      commit_work(replay, *work, &recorded_date(&items(server)), &mut out);
    }
  }
  out
}

/// Asserts that `answered` is what `svnserve` wrote last on the connection.
fn check_tail(name: &str, server: &[u8], answered: &[u8]) {
  let server = items(server);
  let count = items(answered).len();
  let tail: Vec<u8> = server[server.len() - count..]
    .iter()
    .flat_map(|(_, bytes)| bytes.iter().copied())
    .collect();
  assert_eq!(
    String::from_utf8_lossy(answered),
    String::from_utf8_lossy(&tail),
    "{name}"
  );
}

/// Takes bob's lock as `svnserve` did, with the token it drew (A-602's guard over the token's UUID).
fn take_lock(replay: &mut Replay, server: &[u8]) {
  let token = items(server)
    .iter()
    .find_map(|(item, _)| find_token(item))
    .expect("the lock's answer carries its token");
  let uuid: Vec<u8> = token
    .strip_prefix("opaquelocktoken:")
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
  let records = &replay.repository;
  let decision = records
    .locks()
    .lock(
      &request,
      &Head(records.index()),
      AprTime(0),
      ROOM,
      &mut draw,
    )
    .unwrap();
  replay
    .repository
    .apply_lock_entries(&decision.entries)
    .unwrap();
  let held = replay
    .repository
    .locks()
    .get_lock(LOCKED, AprTime(0))
    .unwrap();
  assert_eq!(
    held.token.as_str(),
    token,
    "the drawn token is the recorded one"
  );
}

/// The first lock token in `item`.
fn find_token(item: &Value<'_>) -> Option<String> {
  match item {
    Value::String(bytes) if bytes.starts_with(b"opaquelocktoken:") => {
      String::from_utf8(bytes.to_vec()).ok()
    }
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

/// The names of the revision properties `svnserve` recorded for every commit the recordings hold.
/// Shape: `svn:author`, `svn:date` and `svn:log`, in name order; never the ephemeral `svn:txn-` ones the client sent.
const RECORDED_PROPERTIES: [&[u8]; 3] = [b"svn:author", b"svn:date", b"svn:log"];

/// Checks what `repository` records of `revision`: exactly [`RECORDED_PROPERTIES`], its date `date`.
fn check_recorded(repository: &Repository, revision: u64, date: &str) {
  let properties = repository.revision_properties(revision).unwrap();
  let names: Vec<&[u8]> = properties.keys().map(Vec::as_slice).collect();
  assert_eq!(names, RECORDED_PROPERTIES, "r{revision}");
  let dated = properties.get(b"svn:date".as_slice()).map(Vec::as_slice);
  assert_eq!(dated, Some(date.as_bytes()), "r{revision}");
}

/// A-617, A-655, A-661 (§4.17.4 items 3.2 to 3.5; `commit.c`, `editorp.c`, `serve.c` `commit`, `fs-wrap.c`
/// `svn_repos_fs_commit_txn` 48-121, `transaction.c` `write_final_revprop` 3598-3630). Do: replay every recorded
/// commit of stock svn 1.15.0 against stock svnserve 1.15.0 in its order, bob's lock taken with its recorded token,
/// each hook passed as it comes due and bob's race landing while alice's `close-edit` waits for pre-commit, each
/// accepted work committed through the commit step at the instant `svnserve` dated it. Expect: every answer from
/// `commit` on is the bytes `svnserve` wrote: thirteen commits answered with their revisions 1 to 13, their dates and
/// authors, the race's among them, the stale file and the stale root out of date (`E160028`), alice against bob's
/// lock refused (`E160039`) at the call that meets it, and alice's raced commit a conflict at bob's file (`E160024`),
/// met by the verdict after pre-commit; the youngest is 13; each revision records `svn:author`, `svn:log` and
/// `svnserve`'s `svn:date` alone, the client's `svn:txn-` properties dropped; and bob's commit released his lock.
#[test]
fn stock_commits_are_answered_as_svnserve_answered_them() {
  let replay = replayed_until("");
  assert_eq!(replay.repository.youngest(), 13);
  assert_eq!(replay.dates.len(), 13);
  for (revision, date) in &replay.dates {
    check_recorded(&replay.repository, *revision, date);
  }
  assert_eq!(replay.repository.locks().locks().count(), 0);
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
  Accepted,
  Refused,
  Dropped,
  Ended,
  Hook,
  Close,
}

/// The step's kind.
fn kind_of(step: &Step) -> Last {
  match step {
    Step::Taken => Last::Taken,
    Step::Accepted(_) => Last::Accepted,
    Step::Refused => Last::Refused,
    Step::Dropped => Last::Dropped,
    Step::Ended => Last::Ended,
    Step::Hook(_) => Last::Hook,
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
    locks: CommitLocks {
      user: Some("alice".to_owned()),
      ..CommitLocks::default()
    },
    now: AprTime(0),
    room: ROOM,
    rules: None,
    revision_properties: Vec::new(),
  };
  let mut drive = Drive::new(setup);
  let mut out = Vec::new();
  let mut last = Last::Taken;
  for (item, _) in items(case.commands) {
    last = kind_of(&replay.command(&mut drive, &item, &mut out));
  }
  let failure = items(&out)
    .into_iter()
    .find_map(|(item, _)| failure_of(&item));
  let expected = case
    .failure
    .map(|(code, words)| (u64::from(code), words.to_owned()));
  assert_eq!((failure, last), (expected, case.last), "{}", case.name);
}

/// The number and words of a failure item.
fn failure_of(item: &Value<'_>) -> Option<(u64, String)> {
  let Value::List(parts) = item else {
    return None;
  };
  let [Value::Word("failure"), Value::List(errors)] = parts.as_slice() else {
    return None;
  };
  let Some(Value::List(error)) = errors.first() else {
    return None;
  };
  let [Value::Number(code), Value::String(words), ..] = error.as_slice() else {
    return None;
  };
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
    rules: None,
    revision_properties: Vec::new(),
  });
  let mut out = Vec::new();
  for (item, _) in items(commands) {
    replay.command(&mut drive, &item, &mut out);
  }
  let words = format!(
    "Checksum mismatch for resulting fulltext\n(/trunk/project/a.txt):\n   expected:  00000000000000000000000000000001\n     actual:  {actual}\n"
  );
  let failure = items(&out)
    .into_iter()
    .find_map(|(item, _)| failure_of(&item));
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
    rules: None,
    revision_properties: Vec::new(),
  });
  let mut out = Vec::new();
  for (item, _) in items(commands) {
    replay.command(&mut drive, &item, &mut out);
  }
  let failure = items(&out)
    .into_iter()
    .find_map(|(item, _)| failure_of(&item));
  assert_eq!(
    failure,
    Some((
      210_009,
      "The client request size exceeds the configured limit".to_owned()
    ))
  );
}

/// A-617 (module documentation: locks at each call; `tree.c` `fs_apply_textdelta`). Do: after bob locks `a.txt`, let
/// alice open it and apply a text delta, then send a malformed item. Expect: `E160039` at `apply-textdelta`, before
/// `close-edit`, so the malformed item after it is dropped with the rest of the drive rather than ending the
/// connection.
#[test]
fn a_lock_in_the_way_refuses_the_call_that_meets_it() {
  let replay = replayed_until("locked-by-other");
  let commands = b"( open-root ( ( ) 2:d0 ) ) ( open-file ( 5:a.txt 2:d0 2:c1 ( ) ) ) \
( apply-textdelta ( 2:c1 ( ) ) ) ( 5 ) ";
  let mut drive = Drive::new(Setup {
    base_path: "/trunk/project".to_owned(),
    repository_url: REPOSITORY.to_owned(),
    filesystem: "fixture".to_owned(),
    transaction: "11-1".to_owned(),
    locks: CommitLocks {
      user: Some("alice".to_owned()),
      ..CommitLocks::default()
    },
    now: AprTime(0),
    room: ROOM,
    rules: None,
    revision_properties: Vec::new(),
  });
  let mut out = Vec::new();
  let mut last = Last::Taken;
  for (item, _) in items(commands) {
    last = kind_of(&replay.command(&mut drive, &item, &mut out));
  }
  let failure = items(&out)
    .into_iter()
    .find_map(|(item, _)| failure_of(&item));
  let words =
    "User 'alice' does not own lock on path '/trunk/project/a.txt' (currently locked by 'bob')";
  assert_eq!(
    (failure, last),
    (Some((160_039, words.to_owned())), Last::Dropped)
  );
}

/// The instant bob's lock on `a.txt` expires in the expiry test: a second past the epoch. The lock
/// binds at it and has lapsed a microsecond later (`lock.c` `lock_expired`: past it).
/// Format: `apr_time_t`, microseconds since the Unix epoch.
const LOCK_EXPIRES: AprTime = AprTime(1_000_000);

/// A microsecond past [`LOCK_EXPIRES`].
/// Format: `apr_time_t`, microseconds since the Unix epoch.
const PAST_LOCK_EXPIRY: AprTime = AprTime(1_000_001);

/// bob's lock on `a.txt` made to expire at [`LOCK_EXPIRES`]: the lock the replay holds taken away
/// and set again with its token and that expiry. Answers the token.
fn expiring_lock(replay: &mut Replay) -> String {
  let held = replay
    .repository
    .locks()
    .get_lock(LOCKED, AprTime(0))
    .unwrap()
    .clone();
  let token = held.token.as_str().to_owned();
  let expiring = Lock {
    expires: Some(LOCK_EXPIRES),
    ..held.clone()
  };
  replay
    .repository
    .apply_lock_entries(&[
      LockEntry::Removed {
        path: held.path,
        token: held.token,
      },
      LockEntry::Set(expiring),
    ])
    .unwrap();
  token
}

/// alice opening `a.txt` and applying a text delta to it while presenting `token` for it, with
/// locks judged at `now` (the drive's `Setup::now`, a commit's `CommitStart::now`): the refusal
/// met, if any, and how the last call went.
fn presenting_the_token_at(
  replay: &Replay,
  token: &str,
  now: AprTime,
) -> (Option<(u64, String)>, Last) {
  let commands = b"( open-root ( ( ) 2:d0 ) ) ( open-file ( 5:a.txt 2:d0 2:c1 ( ) ) ) \
( apply-textdelta ( 2:c1 ( ) ) ) ";
  let mut drive = Drive::new(Setup {
    base_path: "/trunk/project".to_owned(),
    repository_url: REPOSITORY.to_owned(),
    filesystem: "fixture".to_owned(),
    transaction: "11-1".to_owned(),
    locks: CommitLocks {
      user: Some("alice".to_owned()),
      presented: BTreeMap::from([(LOCKED.to_owned(), token.to_owned())]),
      keep: false,
    },
    now,
    room: ROOM,
    rules: None,
    revision_properties: Vec::new(),
  });
  let mut out = Vec::new();
  let mut last = Last::Taken;
  for (item, _) in items(commands) {
    last = kind_of(&replay.command(&mut drive, &item, &mut out));
  }
  let failure = items(&out)
    .into_iter()
    .find_map(|(item, _)| failure_of(&item));
  (failure, last)
}

/// A-656 continued (3), a commit's lock instant (FSFS's `svn_fs_fs__allow_locked_operation`, which
/// takes an expired lock for none, at the caller's `CommitStart::now`). Do: with bob's lock on
/// `a.txt` expiring at [`LOCK_EXPIRES`], let alice, presenting its token, open `a.txt` and apply a
/// text delta, with locks judged at the expiry and a microsecond past it. Expect: `E160039` at the
/// expiry, the lock still bob's, `apply-textdelta` refused, and no failure past it, the call taken.
#[test]
fn a_commit_naming_a_lock_meets_it_until_it_expires_and_not_after() {
  let mut replay = replayed_until("locked-by-other");
  let token = expiring_lock(&mut replay);
  let at_expiry = presenting_the_token_at(&replay, &token, LOCK_EXPIRES);
  let past_expiry = presenting_the_token_at(&replay, &token, PAST_LOCK_EXPIRY);
  let words =
    "User 'alice' does not own lock on path '/trunk/project/a.txt' (currently locked by 'bob')";
  assert_eq!(
    (at_expiry, past_expiry),
    (
      (Some((160_039, words.to_owned())), Last::Refused),
      (None, Last::Taken)
    )
  );
}

/// The rows of an accepted drive: each changed path with its action.
fn accepted_rows(step: &Step) -> Vec<(String, Action)> {
  let Step::Accepted(work) = step else {
    return Vec::new();
  };
  let rows = work.record.changes.iter();
  rows.map(|row| (row.path.clone(), row.action)).collect()
}

/// A-617 (module documentation: the transaction; `tree.c` `open_path` through a copy). Do: after `edit`, copy
/// `/trunk/project/sub` to `sub2`, open `sub2/b.txt` at revision 3 with the source's MD5 as its base, empty it, close
/// it with the empty text's MD5 and close the edit. Expect: no refusal: the file beneath the copy is the source's (its
/// created revision 2, its MD5), and the verdict accepts the copy and the file's change.
#[test]
fn a_path_beneath_a_copy_reads_the_copys_source() {
  let replay = replayed_until("edit-large");
  let source = replay
    .repository
    .index()
    .node("/trunk/project/sub/b.txt", 3)
    .unwrap()
    .unwrap();
  let base = displayed(&source.text().unwrap().md5);
  let empty = displayed(&md5_of(b"").unwrap());
  let commands = format!(
    "( open-root ( ( 3 ) 2:d0 ) ) \
     ( add-dir ( 4:sub2 2:d0 2:d1 ( 44:{REPOSITORY}/trunk/project/sub 3 ) ) ) \
     ( open-file ( 10:sub2/b.txt 2:d1 2:c2 ( 3 ) ) ) ( apply-textdelta ( 2:c2 ( 32:{base} ) ) ) \
     ( textdelta-chunk ( 2:c2 4:SVN\0 ) ) ( textdelta-end ( 2:c2 ) ) \
     ( close-file ( 2:c2 ( 32:{empty} ) ) ) ( close-dir ( 2:d1 ) ) ( close-dir ( 2:d0 ) ) \
     ( close-edit ( ) ) "
  );
  let mut drive = Drive::new(Setup {
    base_path: "/trunk/project".to_owned(),
    repository_url: REPOSITORY.to_owned(),
    filesystem: "fixture".to_owned(),
    transaction: "3-1".to_owned(),
    locks: CommitLocks::default(),
    now: AprTime(0),
    room: ROOM,
    rules: None,
    revision_properties: Vec::new(),
  });
  let mut out = Vec::new();
  let mut rows = Vec::new();
  for (item, _) in items(commands.as_bytes()) {
    rows = accepted_rows(&replay.command(&mut drive, &item, &mut out));
  }
  let expected = vec![
    ("/trunk/project/sub2".to_owned(), Action::Add),
    ("/trunk/project/sub2/b.txt".to_owned(), Action::Modify),
  ];
  assert_eq!(
    (String::from_utf8_lossy(&out).into_owned(), rows),
    (String::new(), expected)
  );
}

/// The authorization rules the authorization cases run under: everyone reads, alice writes, but only reads beneath
/// `sub` and cannot read `gone`.
const RULES: &[u8] =
  b"[/]\n* = r\nalice = rw\n[/trunk/project/sub]\nalice = r\n[/trunk/project/gone]\nalice =\n";

/// A drive's case: its commands, whether it runs under [`RULES`], the failure's errors it writes, and its last step.
struct Check {
  /// What the case does.
  name: &'static str,
  /// The editor commands.
  commands: &'static [u8],
  /// Whether alice runs under [`RULES`]; otherwise the repository names no authz file.
  authz: bool,
  /// The failure's errors, each number and words, outermost first; `None` for no failure.
  failure: Option<&'static [(u32, &'static str)]>,
  /// How the last step ends.
  last: Last,
}

/// The commit's remaining checks (A-655), each over the repository after `edit` (revision 3), at `/trunk/project`.
const CHECKS: &[Check] = &[
  Check {
    name: "a control character in an added path",
    commands: b"( open-root ( ( 3 ) 2:d0 ) ) ( add-file ( 3:a\x01b 2:d0 2:c1 ( ) ) ) ",
    authz: false,
    failure: Some(&[(
      160_005,
      "Invalid control character '0x01' in path 'a\\001b'",
    )]),
    last: Last::Refused,
  },
  Check {
    name: "a non-regular property",
    commands: b"( open-root ( ( 3 ) 2:d0 ) ) ( open-file ( 5:a.txt 2:d0 2:c1 ( 3 ) ) ) \
( change-file-prop ( 2:c1 14:svn:entry:uuid ( 1:x ) ) ) ",
    authz: false,
    failure: Some(&[(
      165_002,
      "Storage of non-regular property 'svn:entry:uuid' is disallowed through the repository interface, and could \
indicate a bug in your client",
    )]),
    last: Last::Refused,
  },
  Check {
    name: "a non-regular property deleted",
    commands: b"( open-root ( ( 3 ) 2:d0 ) ) ( open-file ( 5:a.txt 2:d0 2:c1 ( 3 ) ) ) \
( change-file-prop ( 2:c1 14:svn:entry:uuid ( ) ) ) ",
    authz: false,
    failure: None,
    last: Last::Taken,
  },
  Check {
    name: "an svn: property that is not UTF-8",
    commands: b"( open-root ( ( 3 ) 2:d0 ) ) ( open-file ( 5:a.txt 2:d0 2:c1 ( 3 ) ) ) \
( change-file-prop ( 2:c1 13:svn:eol-style ( 2:\xff\xfe ) ) ) ",
    authz: false,
    failure: Some(&[(
      125_005,
      "Cannot accept 'svn:eol-style' property because it is not encoded in UTF-8",
    )]),
    last: Last::Refused,
  },
  Check {
    name: "an svn: property with a carriage return",
    commands:
      b"( open-root ( ( 3 ) 2:d0 ) ) ( change-dir-prop ( 2:d0 10:svn:ignore ( 3:a\r\n ) ) ) ",
    authz: false,
    failure: Some(&[
      (125_005, "Invalid property value"),
      (
        125_017,
        "Cannot accept non-LF line endings in 'svn:ignore' property",
      ),
    ]),
    last: Last::Refused,
  },
  Check {
    name: "a regular property with a carriage return",
    commands: b"( open-root ( ( 3 ) 2:d0 ) ) ( change-dir-prop ( 2:d0 6:colour ( 3:a\r\n ) ) ) ",
    authz: false,
    failure: None,
    last: Last::Taken,
  },
  Check {
    name: "an svn:date that is not a date",
    commands: b"( open-root ( ( 3 ) 2:d0 ) ) ( change-dir-prop ( 2:d0 8:svn:date ( 3:bad ) ) ) ",
    authz: false,
    failure: Some(&[
      (125_005, "Wrong or unexpected property value"),
      (125_003, "Bogus date"),
    ]),
    last: Last::Refused,
  },
  Check {
    name: "an svn:date that is one",
    commands: b"( open-root ( ( 3 ) 2:d0 ) ) \
( change-dir-prop ( 2:d0 8:svn:date ( 27:2026-11-19T05:05:34.384728Z ) ) ) ",
    authz: false,
    failure: None,
    last: Last::Taken,
  },
  Check {
    name: "mergeinfo holding a NUL",
    commands:
      b"( open-root ( ( 3 ) 2:d0 ) ) ( change-dir-prop ( 2:d0 13:svn:mergeinfo ( 6:/a:1\x00x ) ) ) ",
    authz: false,
    failure: Some(&[(
      200_020,
      "Commit rejected because mergeinfo on '/trunk/project' contains unexpected string terminator",
    )]),
    last: Last::Refused,
  },
  Check {
    name: "mergeinfo that does not parse",
    commands:
      b"( open-root ( ( 3 ) 2:d0 ) ) ( change-dir-prop ( 2:d0 13:svn:mergeinfo ( 5:/a:xx ) ) ) ",
    authz: false,
    failure: Some(&[(
      200_020,
      "Commit rejected because mergeinfo on '/trunk/project' is syntactically invalid",
    )]),
    last: Last::Refused,
  },
  Check {
    name: "a deletion of a directory alice only reads",
    commands: b"( open-root ( ( 3 ) 2:d0 ) ) ( delete-entry ( 3:sub ( 3 ) 2:d0 ) ) ",
    authz: true,
    failure: Some(&[(220_004, "Access denied")]),
    last: Last::Refused,
  },
  Check {
    name: "a text change of a file alice only reads",
    commands: b"( open-root ( ( 3 ) 2:d0 ) ) ( open-file ( 9:sub/b.txt 2:d0 2:c1 ( 3 ) ) ) \
( apply-textdelta ( 2:c1 ( ) ) ) ",
    authz: true,
    failure: Some(&[(220_004, "Access denied")]),
    last: Last::Refused,
  },
  Check {
    name: "a copy from a path alice cannot read",
    commands: b"( open-root ( ( 3 ) 2:d0 ) ) \
( add-dir ( 1:x 2:d0 2:d1 ( 45:svn+fixture://fixture/repo/trunk/project/gone 3 ) ) ) ",
    authz: true,
    failure: Some(&[(220_001, "Access denied")]),
    last: Last::Refused,
  },
  Check {
    name: "an open of a file alice reads",
    commands: b"( open-root ( ( 3 ) 2:d0 ) ) ( open-file ( 9:sub/b.txt 2:d0 2:c1 ( 3 ) ) ) ",
    authz: true,
    failure: None,
    last: Last::Taken,
  },
  Check {
    name: "a property of a directory alice writes",
    commands: b"( open-root ( ( 3 ) 2:d0 ) ) ( change-dir-prop ( 2:d0 6:colour ( 4:blue ) ) ) ",
    authz: true,
    failure: None,
    last: Last::Taken,
  },
];

/// Every error of a failure item, outermost first.
fn errors_of(item: &Value<'_>) -> Option<Vec<(u64, String)>> {
  let Value::List(parts) = item else {
    return None;
  };
  let [Value::Word("failure"), Value::List(errors)] = parts.as_slice() else {
    return None;
  };
  let read = |error: &Value<'_>| {
    let Value::List(error) = error else {
      return None;
    };
    let [Value::Number(code), Value::String(words), ..] = error.as_slice() else {
      return None;
    };
    Some((*code, String::from_utf8_lossy(words).into_owned()))
  };
  errors.iter().map(read).collect()
}

/// Alice's rules under [`RULES`].
fn alice_rules() -> authz::PrincipalRules {
  let rules = authz::Authz::parse(RULES, None, authz::Bounds::of(RULES, None)).unwrap();
  rules.rules(b"repo", authz::Principal::named(Some(b"alice")))
}

/// Runs one case over `replay` and checks it.
fn check_case(replay: &Replay, case: &Check) {
  let setup = Setup {
    base_path: "/trunk/project".to_owned(),
    repository_url: REPOSITORY.to_owned(),
    filesystem: "fixture".to_owned(),
    transaction: "3-1".to_owned(),
    locks: CommitLocks {
      user: Some("alice".to_owned()),
      ..CommitLocks::default()
    },
    now: AprTime(0),
    room: ROOM,
    rules: case.authz.then(alice_rules),
    revision_properties: Vec::new(),
  };
  let mut drive = Drive::new(setup);
  let mut out = Vec::new();
  let mut last = Last::Taken;
  for (item, _) in items(case.commands) {
    last = kind_of(&replay.command(&mut drive, &item, &mut out));
  }
  let failure = items(&out)
    .into_iter()
    .find_map(|(item, _)| errors_of(&item));
  let expected = case.failure.map(|errors| {
    let owned = errors
      .iter()
      .map(|(code, words)| (u64::from(*code), (*words).to_owned()));
    owned.collect::<Vec<_>>()
  });
  assert_eq!((failure, last), (expected, case.last), "{}", case.name);
}

/// A-655 (`fs-wrap.c` `svn_repos__validate_prop` 202-263 and `verify_mergeinfo` 297-324; `repos.c`
/// `svn_repos__validate_new_path` 2097, `path.c` `svn_path_check_valid`; `commit.c` `check_authz` 243-262 and its
/// calls). Do: drive the repository after `edit` with each case. Expect: an added path with a control character, a
/// non-regular property, an `svn:` property not in UTF-8 or with a carriage return, an `svn:date` that is no date and
/// mergeinfo that holds a NUL or does not parse each refused with every error `svnserve` writes, in its numbers and
/// words; a write or a read the rules deny refused "Access denied"; and the deletions, values and accesses Subversion
/// allows taken.
#[test]
fn the_commits_remaining_checks_refuse_as_svnserve_refuses() {
  let replay = replayed_until("edit-large");
  for case in CHECKS {
    check_case(&replay, case);
  }
}

/// A session as the recordings' `svnserve.conf` makes one (`make-fixtures.py`: `anon-access = none`, `auth-access =
/// write`, the tunnel's user authenticating).
fn recorded_session<'s>(
  base_path: &'s str,
  user: Option<&'s str>,
  authz: Option<(&'s authz::Authz, &'s [u8])>,
) -> Session<'s> {
  Session {
    base_path,
    repository_url: REPOSITORY,
    filesystem: "fixture",
    user,
    anonymous: authz::Access::None,
    authenticated: authz::Access::Write,
    can_authenticate: true,
    authz,
  }
}

/// A `commit`'s parameters: a log message, the lock tokens given, keep-locks false, no revision properties.
fn commit_params(tokens: &[(&'static [u8], &'static [u8])]) -> Vec<Value<'static>> {
  let pairs = tokens
    .iter()
    .map(|(path, token)| Value::List(vec![Value::String(path), Value::String(token)]));
  vec![
    Value::String(b"log"),
    Value::List(pairs.collect()),
    Value::Word("false"),
  ]
}

/// A start of the transaction `3-1`.
fn start(authenticated_now: bool) -> Start {
  Start {
    transaction: "3-1".to_owned(),
    now: AprTime(0),
    room: ROOM,
    authenticated_now,
  }
}

/// What a commit began as, every error of the failure it wrote, if any, and what it wrote.
type Began = (&'static str, Option<Vec<(u64, String)>>, Vec<u8>);

/// Begins a commit on `session` with `params`.
fn begun(session: &Session<'_>, params: &[Value<'_>], now: bool) -> Began {
  let mut out = Vec::new();
  let kind = match commit(params, session, start(now), &mut out).unwrap() {
    Begun::Driving(_) => "driving",
    Begun::Authenticate => "authenticate",
    Begun::Answered => "answered",
    Begun::Close => "close",
  };
  let failure = items(&out)
    .into_iter()
    .find_map(|(item, _)| errors_of(&item));
  (kind, failure, out)
}

/// A-655 (`serve.c` `must_have_access` 807-850, `lookup_access` 760-791). Do: begin a commit on an anonymous session
/// whose `svnserve.conf` lets only the authenticated write, then again as alice once she authenticated. Expect: the
/// first asks the client to authenticate, writing nothing; the second drives, writing `( success ( ) )` alone, no
/// trivial authentication request after a real one.
#[test]
fn an_anonymous_session_authenticates_for_write_before_its_commit() {
  let params = commit_params(&[]);
  let anonymous = recorded_session("/trunk/project", None, None);
  assert_eq!(
    begun(&anonymous, &params, false),
    ("authenticate", None, Vec::new())
  );
  let alice = recorded_session("/trunk/project", Some("alice"), None);
  let driving = ("driving", None, b"( success ( ) ) ".to_vec());
  assert_eq!(begun(&alice, &params, true), driving);
}

/// The rules of `text`, read whole.
fn ruled(text: &'static [u8]) -> authz::Authz {
  authz::Authz::parse(text, None, authz::Bounds::of(text, None)).unwrap()
}

/// A-655 (`serve.c` 840-844 and `add_lock_tokens` 1388-1390). Do: begin commits as alice where she may not write: her
/// `auth-access` only reads; the rules let her read alone; a lock token on a file the rules let her read alone.
/// Expect: each refused "Authorization failed" (`E170001`), the last after the trivial authentication request.
#[test]
fn a_commit_alice_may_not_write_is_refused_authorization() {
  let refused = Some(vec![(170_001, "Authorization failed".to_owned())]);
  let mut reading = recorded_session("/trunk/project", Some("alice"), None);
  reading.authenticated = authz::Access::Read;
  let (kind, failure, _) = begun(&reading, &commit_params(&[]), false);
  assert_eq!((kind, failure), ("answered", refused.clone()));
  let read_only = ruled(b"[/]\nalice = r\n");
  let only = recorded_session("/trunk/project", Some("alice"), Some((&read_only, b"repo")));
  assert_eq!(begun(&only, &commit_params(&[]), false).1, refused.clone());
  let file = ruled(b"[/]\nalice = rw\n[/trunk/project/a.txt]\nalice = r\n");
  let locked = recorded_session("/trunk/project", Some("alice"), Some((&file, b"repo")));
  let token = commit_params(&[(b"a.txt", b"opaquelocktoken:x")]);
  let (kind, failure, out) = begun(&locked, &token, false);
  assert_eq!((kind, failure), ("answered", refused));
  assert!(out.starts_with(b"( success ( ( ) 0: ) ) "));
}

/// A property change of the session's directory over the repository after `edit` (revision 3), then `close-edit`.
const DIRECTORY_PROPERTY: &[u8] = b"( open-root ( ( 3 ) 2:d0 ) ) \
( change-dir-prop ( 2:d0 6:colour ( 4:blue ) ) ) ( close-dir ( 2:d0 ) ) ( close-edit ( ) ) ";

/// Alice's drive at `base_path` as transaction `3-1`, beginning with `revision_properties`.
fn alice_setup(base_path: &str, revision_properties: Vec<(Vec<u8>, Vec<u8>)>) -> Setup {
  Setup {
    base_path: base_path.to_owned(),
    repository_url: REPOSITORY.to_owned(),
    filesystem: "fixture".to_owned(),
    transaction: "3-1".to_owned(),
    locks: CommitLocks {
      user: Some("alice".to_owned()),
      ..CommitLocks::default()
    },
    now: AprTime(0),
    room: ROOM,
    rules: None,
    revision_properties,
  }
}

/// The hooks a drive of `commands` over `replay` waited for, each passed as it came due, and how its last step ended.
fn hooks_met(
  replay: &Replay,
  drive: &mut Drive,
  commands: &[u8],
  out: &mut Vec<u8>,
) -> (Vec<Due>, Last) {
  let mut dues = Vec::new();
  let mut last = Last::Taken;
  for (item, _) in items(commands) {
    let records = &replay.repository;
    let mut step = drive
      .command(
        records.index(),
        records,
        records.locks(),
        &replay.texts,
        &item,
        out,
      )
      .unwrap();
    if let Step::Hook(due) = step {
      dues.push(due);
      step = replay.resume(drive, Ok(()), out);
    }
    last = kind_of(&step);
  }
  (dues, last)
}

/// A-661 (`commit.c` `open_root` 437-441 and `close_edit` 816; `fs-wrap.c` `svn_repos_fs_begin_txn_for_commit2`
/// 151-178 and `svn_repos_fs_commit_txn` 69-71). Do: drive a property change of `/trunk/project` over the repository
/// after `edit`, passing each hook as it comes due. Expect: `open-root` waits for start-commit and `close-edit` for
/// pre-commit, each naming the transaction and writing nothing, and the drive goes on after each to its acceptance.
#[test]
fn start_commit_and_pre_commit_are_waited_for_where_svnserve_runs_them() {
  let replay = replayed_until("edit-large");
  let mut drive = Drive::new(alice_setup("/trunk/project", Vec::new()));
  let mut out = Vec::new();
  let met = hooks_met(&replay, &mut drive, DIRECTORY_PROPERTY, &mut out);
  let transaction = "3-1".to_owned();
  let dues = vec![
    Due::StartCommit {
      transaction: transaction.clone(),
    },
    Due::PreCommit { transaction },
  ];
  assert_eq!((met, out), ((dues, Last::Accepted), Vec::new()));
}

/// The errors a hook refused with, as the hook glue hands them over.
fn hook_errors(hook: &str) -> Vec<(u32, String)> {
  vec![
    (
      165_001,
      format!("Commit blocked by {hook} hook (exit code 1) with output:\nno"),
    ),
    (165_001, "and a second error beneath it".to_owned()),
  ]
}

/// The failure a drive wrote, every error of it, and how the drive ended: `commands` handed in, the hook at `refused`
/// answering with [`hook_errors`] and every other hook passing, then `abort-edit`.
fn refused_by_hook(replay: &Replay, refused: &str) -> (Option<Vec<(u64, String)>>, Vec<Last>) {
  let mut drive = Drive::new(alice_setup("/trunk/project", Vec::new()));
  let mut out = Vec::new();
  let mut steps = Vec::new();
  for (item, _) in items(DIRECTORY_PROPERTY)
    .into_iter()
    .chain(items(b"( abort-edit ( ) ) "))
  {
    let records = &replay.repository;
    let mut step = drive
      .command(
        records.index(),
        records,
        records.locks(),
        &replay.texts,
        &item,
        &mut out,
      )
      .unwrap();
    if let Step::Hook(due) = step {
      let hook = match due {
        Due::StartCommit { .. } => "start-commit",
        Due::PreCommit { .. } => "pre-commit",
      };
      let answer = if hook == refused {
        Err(hook_errors(hook))
      } else {
        Ok(())
      };
      step = replay.resume(&mut drive, answer, &mut out);
    }
    steps.push(kind_of(&step));
  }
  let failure = items(&out)
    .into_iter()
    .find_map(|(item, _)| errors_of(&item));
  (failure, steps)
}

/// The errors `hook`'s refusal is written with.
fn written(hook: &str) -> Option<Vec<(u64, String)>> {
  let errors = hook_errors(hook).into_iter();
  Some(
    errors
      .map(|(code, words)| (u64::from(code), words))
      .collect(),
  )
}

/// A-661 (`fs-wrap.c` 172-178 and 69-71; `editorp.c` 1149-1208). Do: drive the property change of `/trunk/project`
/// with start-commit refusing, then again with start-commit passing and pre-commit refusing, `abort-edit` last each
/// time. Expect: the refusal written as the failure of the command that waited, every error the hook gave in its
/// order; each item after it dropped until `abort-edit` ends the drive; nothing accepted.
#[test]
fn a_hook_that_refuses_is_the_failure_of_the_command_that_waited() {
  let replay = replayed_until("edit-large");
  let (failure, steps) = refused_by_hook(&replay, "start-commit");
  let after_start = vec![
    Last::Refused,
    Last::Dropped,
    Last::Dropped,
    Last::Dropped,
    Last::Ended,
  ];
  assert_eq!((failure, steps), (written("start-commit"), after_start));
  let (failure, steps) = refused_by_hook(&replay, "pre-commit");
  let after_pre = vec![
    Last::Taken,
    Last::Taken,
    Last::Taken,
    Last::Refused,
    Last::Ended,
  ];
  assert_eq!((failure, steps), (written("pre-commit"), after_pre));
}

/// A drive of `open-root` alone with `revision_properties`: the failure it wrote, if any, and its step.
fn opened_with(
  replay: &Replay,
  revision_properties: &[(&[u8], &[u8])],
) -> (Option<Vec<(u64, String)>>, Last) {
  let owned = revision_properties
    .iter()
    .map(|(name, value)| (name.to_vec(), value.to_vec()))
    .collect();
  let mut drive = Drive::new(alice_setup("/trunk/project", owned));
  let mut out = Vec::new();
  let mut last = Last::Taken;
  for (item, _) in items(b"( open-root ( ( 3 ) 2:d0 ) ) ") {
    let records = &replay.repository;
    let step = drive
      .command(
        records.index(),
        records,
        records.locks(),
        &replay.texts,
        &item,
        &mut out,
      )
      .unwrap();
    last = kind_of(&step);
  }
  let failure = items(&out)
    .into_iter()
    .find_map(|(item, _)| errors_of(&item));
  (failure, last)
}

/// A-655 (`fs-wrap.c` `svn_repos_fs_change_txn_props` 344-357, which `svn_repos_fs_begin_txn_for_commit2` calls at
/// 160-164 before start-commit; `svn_repos__validate_prop` 202-263). Do: begin drives whose revision properties hold an
/// `svn:log` with a carriage return, an `svn:date` that is no date, and a regular property with a carriage return.
/// Expect: the first two refused at `open-root` with every error `svnserve` writes, no hook due; the third waits for
/// start-commit.
#[test]
fn a_revision_property_svnserve_refuses_is_refused_at_open_root() {
  let replay = replayed_until("edit-large");
  let carriage = opened_with(&replay, &[(b"svn:log".as_slice(), b"a\r\n".as_slice())]);
  let eol = vec![
    (125_005, "Invalid property value".to_owned()),
    (
      125_017,
      "Cannot accept non-LF line endings in 'svn:log' property".to_owned(),
    ),
  ];
  assert_eq!(carriage, (Some(eol), Last::Refused));
  let dated = opened_with(
    &replay,
    &[
      (b"svn:date".as_slice(), b"bad".as_slice()),
      (b"svn:log".as_slice(), b"ok".as_slice()),
    ],
  );
  let bogus = vec![
    (125_005, "Wrong or unexpected property value".to_owned()),
    (125_003, "Bogus date".to_owned()),
  ];
  assert_eq!(dated, (Some(bogus), Last::Refused));
  let regular = opened_with(&replay, &[(b"colour".as_slice(), b"a\r\n".as_slice())]);
  assert_eq!(regular, (None, Last::Hook));
}

/// A-661 (`Drive::command`, `Drive::resume`). Do: hand a drive its next item while start-commit is due; and resume a
/// drive no hook paused. Expect: each ends the drive, nothing written.
#[test]
fn an_item_while_a_hook_is_due_or_a_resume_with_none_ends_the_drive() {
  let replay = replayed_until("edit-large");
  let records = &replay.repository;
  let mut drive = Drive::new(alice_setup("/trunk/project", Vec::new()));
  let mut out = Vec::new();
  let mut steps = Vec::new();
  for (item, _) in items(DIRECTORY_PROPERTY).into_iter().take(2) {
    let step = drive.command(
      records.index(),
      records,
      records.locks(),
      &replay.texts,
      &item,
      &mut out,
    );
    steps.push(kind_of(&step.unwrap()));
  }
  let mut idle = Drive::new(alice_setup("/trunk/project", Vec::new()));
  steps.push(kind_of(&replay.resume(&mut idle, Ok(()), &mut out)));
  assert_eq!(
    (steps, out),
    (vec![Last::Hook, Last::Close, Last::Close], Vec::new())
  );
}

/// The work a drive of `commands` at `base_path` over `replay` accepted, each hook passed.
fn accepted_work(replay: &Replay, base_path: &str, commands: &[u8]) -> Work {
  let mut drive = Drive::new(alice_setup(base_path, Vec::new()));
  let mut out = Vec::new();
  let mut accepted = None;
  for (item, _) in items(commands) {
    if let Step::Accepted(work) = replay.command(&mut drive, &item, &mut out) {
      accepted = Some(*work);
    }
  }
  accepted.unwrap()
}

/// A-655 (§4.17.4 item 3.5; `transaction.c` `commit_body` 3784-3786). Do: over the repository after `edit`, accept a
/// property change of `/trunk/project` and one of `/trunk/project/sub`, both judged at revision 3; commit the second
/// through the step, then the first as it was judged. Expect: the second is revision 4; the records refuse the first as
/// FSFS's `commit_body` refuses a transaction whose base moved, "Transaction out of date" (`E160028`), holding revision
/// 4 alone. The owner's step judges such a work again at the head by position before the records see it, as
/// `svn_fs_fs__commit_txn` merges again (part 2b).
#[test]
fn a_revision_judged_at_a_moved_head_is_refused_by_the_records() {
  let mut replay = replayed_until("edit-large");
  let first = accepted_work(&replay, "/trunk/project", DIRECTORY_PROPERTY);
  let second = accepted_work(&replay, "/trunk/project/sub", DIRECTORY_PROPERTY);
  let now = AprTime(1_795_073_195_123_456);
  let second = RevisionToCommit::of_work(second, now, &replay.texts).unwrap();
  assert_eq!(replay.repository.commit(second), Ok(4));
  let first = RevisionToCommit::of_work(first, now, &replay.texts).unwrap();
  let refusal = replay.repository.commit(first).unwrap_err();
  let answered = (refusal.subversion_code(), refusal.message());
  assert_eq!(answered, (160_028, "Transaction out of date".to_owned()));
  assert_eq!(
    refusal,
    StepRefusal::OutOfDate {
      head: 3,
      youngest: 4
    }
  );
  let changed: Vec<String> = replay
    .repository
    .index()
    .changes(4)
    .unwrap()
    .iter()
    .map(|row| row.path.clone())
    .collect();
  assert_eq!(
    (replay.repository.youngest(), changed),
    (4, vec!["/trunk/project/sub".to_owned()])
  );
}

/// The commit locks of a drive of alice's, as [`alice_setup`] gives them.
fn alices_locks() -> CommitLocks {
  alice_setup("/", Vec::new()).locks
}

/// A-655 part 2b (`tree.c` `svn_fs_fs__commit_txn` 2255-2310: FSFS merges a transaction whose base moved again with the
/// youngest and commits it). Do: over the repository after `edit`, accept a property change of `/trunk/project` and one
/// of `/trunk/project/sub`, both judged at revision 3; commit the second; check the first through the step's check of a
/// work (`check_work`) and apply it. Expect: the first merged again at revision 4, no conflict found (the head changed
/// a node beneath the directory whose properties it changes, keeping its identities), committed as revision 5 on
/// revision 4, its rows its own.
#[test]
fn t_a655_a_work_judged_at_a_moved_head_is_merged_again_and_lands() {
  let mut replay = replayed_until("edit-large");
  let first = accepted_work(&replay, "/trunk/project", DIRECTORY_PROPERTY);
  let second = accepted_work(&replay, "/trunk/project/sub", DIRECTORY_PROPERTY);
  let now = AprTime(1_795_073_195_123_456);
  let second = RevisionToCommit::of_work(second, now, &replay.texts).unwrap();
  assert_eq!(replay.repository.commit(second), Ok(4));
  let first = RevisionToCommit::of_work(first, now, &replay.texts).unwrap();
  let rows = first.record.changes.clone();
  let checked = replay
    .repository
    .check_work(first, &alices_locks(), now)
    .unwrap();
  assert_eq!(checked.revision(), 5);
  assert_eq!(replay.repository.apply(checked), Ok(5));
  let committed = replay.repository.index().changes(5).unwrap().to_vec();
  let mut sorted = rows;
  sorted.sort_by(|left, right| left.path.cmp(&right.path));
  assert_eq!(committed, sorted);
}

/// A-655 part 2b (`tree.c` `merge()` 1946, rule 3, met by the merge again). Do: over the repository after `edit`, accept
/// two property changes of `/trunk/project`, both judged at revision 3; commit the second; check the first through
/// `check_work`. Expect: refused as the drive's verdict refuses it, "Conflict at '/trunk/project'" (`E160024`), the
/// head having changed the directory's property list since; the repository at revision 4.
#[test]
fn t_a655_a_moved_work_that_conflicts_is_refused_in_the_verdicts_words() {
  let mut replay = replayed_until("edit-large");
  let first = accepted_work(&replay, "/trunk/project", DIRECTORY_PROPERTY);
  let second = accepted_work(&replay, "/trunk/project", DIRECTORY_PROPERTY);
  let now = AprTime(1_795_073_195_123_456);
  let second = RevisionToCommit::of_work(second, now, &replay.texts).unwrap();
  assert_eq!(replay.repository.commit(second), Ok(4));
  let first = RevisionToCommit::of_work(first, now, &replay.texts).unwrap();
  let refusal = replay
    .repository
    .check_work(first, &alices_locks(), now)
    .unwrap_err();
  let conflict = CommitRefusal::Conflict {
    path: "/trunk/project".to_owned(),
    kind: crate::commit::ConflictKind::PropertiesChanged,
  };
  let answered = (refusal.subversion_code(), refusal.message());
  assert_eq!(
    answered,
    (160_024, "Conflict at '/trunk/project'".to_owned())
  );
  assert_eq!(
    (refusal, replay.repository.youngest()),
    (
      StepRefusal::Verdict {
        refusal: conflict,
        user: Some("alice".to_owned()),
      },
      4
    )
  );
}

/// What a hook read of the transaction: its name, its base, its revision properties and the paths of its rows.
type HookRead = (String, Revision, Vec<(Vec<u8>, Vec<u8>)>, Vec<String>);

/// What pre-commit reads of a drive of `commands` with `begun` as its revision properties, every hook passed, and the
/// revision properties of the work accepted.
fn read_at_pre_commit(
  replay: &Replay,
  begun: Vec<(Vec<u8>, Vec<u8>)>,
  commands: &[u8],
) -> (Option<HookRead>, Option<PropertyPairs>) {
  let mut drive = Drive::new(alice_setup("/trunk/project", begun));
  let mut out = Vec::new();
  let (mut read, mut accepted) = (None, None);
  for (item, _) in items(commands) {
    let records = &replay.repository;
    let mut step = drive
      .command(
        records.index(),
        records,
        records.locks(),
        &replay.texts,
        &item,
        &mut out,
      )
      .unwrap();
    if let Step::Hook(due) = step {
      if let Due::PreCommit { .. } = due {
        let pending = drive.pending();
        let rows: Vec<String> = pending.rows.keys().cloned().collect();
        let properties = pending.revision_properties.to_vec();
        read = Some((
          pending.transaction.to_owned(),
          pending.base,
          properties,
          rows,
        ));
      }
      step = replay.resume(&mut drive, Ok(()), &mut out);
    }
    if let Step::Accepted(work) = step {
      accepted = Some(work.revision_properties);
    }
  }
  (read, accepted)
}

/// A-661 (`fs-wrap.c` `svn_repos_fs_commit_txn` 69-89: pre-commit reads the transaction, its ephemeral properties
/// among its revision properties, and they are removed before the commit). Do: drive the property change of
/// `/trunk/project`, begun with `svn:log` and `svn:txn-user-agent`, reading the transaction while pre-commit is due.
/// Expect: pre-commit reads the transaction's name, its base 3, both properties and its one row; the accepted work
/// keeps `svn:log` alone.
#[test]
fn pre_commit_reads_the_transaction_whose_work_drops_its_ephemeral_properties() {
  let replay = replayed_until("edit-large");
  let log = (b"svn:log".to_vec(), b"x".to_vec());
  let agent = (b"svn:txn-user-agent".to_vec(), b"SVN/1.15.0".to_vec());
  let begun = vec![log.clone(), agent.clone()];
  let (read, accepted) = read_at_pre_commit(&replay, begun, DIRECTORY_PROPERTY);
  let rows = vec!["/trunk/project".to_owned()];
  let expected = ("3-1".to_owned(), 3, vec![log.clone(), agent], rows);
  assert_eq!((read, accepted), (Some(expected), Some(vec![log])));
}

/// A-730 (`hooks.c` `lock_token_content`; `serve.c` `add_lock_tokens` hands the filesystem each path as the client sent
/// it). Do: read pre-commit's lock tokens off a transaction whose session is based at `/trunk/with space`, presenting
/// tokens for two files beneath it, and off one based at the root. Expect: each path relative to its base, as the
/// client wrote it (`b.txt` for `/trunk/with space/b.txt`, A-623's recording), in path order; at the root, the path
/// without its leading slash.
#[test]
fn pre_commits_lock_tokens_name_each_path_as_the_client_sent_it() {
  let presented: BTreeMap<String, String> = [
    ("/trunk/with space/b.txt", "opaquelocktoken:b"),
    ("/trunk/with space/sub/c.txt", "opaquelocktoken:c"),
  ]
  .into_iter()
  .map(|(path, token)| (path.to_owned(), token.to_owned()))
  .collect();
  let (rows, texts, property_lists) = (
    BTreeMap::new(),
    std::collections::BTreeSet::new(),
    BTreeMap::new(),
  );
  let pending = |base_path| Pending {
    transaction: "0-0",
    base: 0,
    revision_properties: &[],
    rows: &rows,
    texts: &texts,
    property_lists: &property_lists,
    base_path,
    presented: &presented,
  };
  let pair = |path: &str, token: &str| (path.to_owned(), token.to_owned());
  assert_eq!(
    pending("/trunk/with space").lock_tokens(),
    vec![
      pair("b.txt", "opaquelocktoken:b"),
      pair("sub/c.txt", "opaquelocktoken:c")
    ]
  );
  assert_eq!(
    pending("/").lock_tokens(),
    vec![
      pair("trunk/with space/b.txt", "opaquelocktoken:b"),
      pair("trunk/with space/sub/c.txt", "opaquelocktoken:c")
    ]
  );
}

/// Shape: the request room of the large-text test's drive, far below its text: room for the window being read (a
/// window's svndiff arrives whole before it is applied, at most about [`crate::svndiff::WINDOW_LIMIT`] of new data and
/// its instructions) with a chunk of the next, a quarter of the text.
const SMALL_ROOM: usize = 256 << 10;

/// Shape: the large-text test's text, in deltifier windows (`SVN_DELTA_WINDOW_SIZE`): 1 MB, four times the room.
const LARGE_TEXT_WINDOWS: usize = 10;

/// Shape: the bytes of svndiff one `textdelta-chunk` carries in the large-text test, as a client splits a delta.
const CHUNK: usize = 16 << 10;

/// §4.17.1 "A push's intake", rules 1 and 2 (main's order of 2026-11-20: stock svn's 500 MB commit was refused E210009
/// by one request's room, its texts held whole). Do: over an empty repository, with a drive whose request room is
/// [`SMALL_ROOM`], add a file whose [`LARGE_TEXT_WINDOWS`]-window text of noisy bytes arrives as svndiff in [`CHUNK`]
/// pieces, then close the edit. Expect: the work accepted, its one text named by the bytes' BLAKE3 and sealed in the
/// sink whole, no text left staged; the drive never held more than the room. Before, the drive held the text and was
/// refused `Room` (E210009) as the text passed the room.
#[test]
fn a_text_larger_than_the_request_room_is_streamed_into_the_sink_and_accepted() {
  use crate::svndiff::{Compression, Encoder, Version, WINDOW_LIMIT, deltas};
  let replay = Replay::new();
  let window = usize::try_from(WINDOW_LIMIT).unwrap();
  let mut state = 7u64;
  let text: Vec<u8> = (0..LARGE_TEXT_WINDOWS * window)
    .map(|_| {
      state ^= state << 13;
      state ^= state >> 7;
      state ^= state << 17;
      state.to_le_bytes()[0]
    })
    .collect();
  let mut encoder = Encoder::new(Version::Zero);
  let mut svndiff = encoder.header().to_vec();
  let room = std::num::NonZeroUsize::new(window).unwrap();
  for made in deltas(&[], &text, room).unwrap() {
    encoder
      .window(&made, Compression::Stored, &mut svndiff)
      .unwrap();
  }
  let mut commands = b"( open-root ( ( 0 ) 2:d0 ) ) ( add-file ( 7:big.bin 2:d0 2:c1 ( ) ) ) \
( apply-textdelta ( 2:c1 ( ) ) ) "
    .to_vec();
  for piece in svndiff.chunks(CHUNK) {
    commands.extend_from_slice(format!("( textdelta-chunk ( 2:c1 {}:", piece.len()).as_bytes());
    commands.extend_from_slice(piece);
    commands.extend_from_slice(b" ) ) ");
  }
  commands.extend_from_slice(
    b"( textdelta-end ( 2:c1 ) ) ( close-file ( 2:c1 ( ) ) ) ( close-dir ( 2:d0 ) ) ( close-edit ( ) ) ",
  );
  let mut setup = alice_setup("/", Vec::new());
  setup.room = SMALL_ROOM;
  let mut drive = Drive::new(setup);
  let mut out = Vec::new();
  let mut accepted = None;
  for (item, _) in items(&commands) {
    match replay.command(&mut drive, &item, &mut out) {
      Step::Accepted(work) => accepted = Some(*work),
      Step::Refused => panic!("refused: {}", String::from_utf8_lossy(&out)),
      _ => {}
    }
  }
  let work = accepted.expect("the work accepted");
  let identity = *blake3::hash(&text).as_bytes();
  assert_eq!(
    work.texts.iter().copied().collect::<Vec<_>>(),
    vec![identity]
  );
  let sealed = TextSink::sealed(&replay.texts, &identity).map(|bytes| bytes.into_owned());
  assert!(
    sealed.as_deref() == Some(text.as_slice()),
    "the text sealed whole"
  );
  assert_eq!(replay.texts.staged_count(), 0);
}
