//! The connection's tests (A-656; §4.17.4 "5. The wire", piece 7).
//!
//! The oracle is stock `svnserve` 1.15.0's whole sessions recorded for the reporter
//! (`crates/svn/fixtures/report/`, each file's SHA-256 in `SHA256SUMS`): each session's client bytes
//! are handed over an item at a time, each when the connection asks for more, as a stock client
//! sends them after reading each answer; what the connection writes over the fixtures' repository
//! must be what `svnserve` wrote: its greeting less `svndiff1`, every item byte for byte (property
//! lists as sets: `svnserve` writes them in APR's seeded hash order), its drive call for call; up to
//! the first command slates does not serve yet, which must be answered as one `svnserve` does not
//! know. Stock `svnserve` 1.15.0's recorded commits are replayed the same way in `commit`
//! (`crates/svn/fixtures/commit/`, through `svnserve -t`), every item byte for byte. Beside them:
//! `reparent` inside and outside the repository, commands not served yet, items that are no
//! command, calls without the repository's sources, the smallest budget, what the connection waits
//! for, a tunnel's user, a repository that admits no one; the authentication for one command
//! (`must_have_access`) for `update` and `commit`, where it helps and where it cannot; a commit
//! answered with the revision made, and refused at `close-edit` in the drive's words and as not
//! made yet, the client's commands then dropped until `abort-edit`; and calls out of turn.

use super::*;
use crate::index::{Action, Index};
use crate::report::drive::green::texts_by_md5;
use crate::report::drive::green::{FixtureGuards, Green, guards_of, normalize, repository};
use crate::report::recorded::{
  ROOT_URL, Recorded, SESSIONS, UUID, command, drive_items, finished_report, items, read_drive,
  value_of,
};
use std::collections::BTreeMap;

mod commit;

/// A budget past anything one call of the fixtures writes.
const PLENTY: Budget = Budget {
  room: 1 << 24,
  steps: 1 << 20,
};

/// The smallest budget: one byte and one step a call.
const SMALLEST: Budget = Budget { room: 1, steps: 1 };

/// Limits past anything the fixtures hold.
const LIMITS: Limits = Limits {
  item_bytes: 1 << 20,
  report_room: 1 << 20,
  most_open: 64,
};

/// The word slates leaves out of its greeting (A-599).
const SVNDIFF1: &[u8] = b"svndiff1 ";

/// The sessions the golden replay must meet: every recorded one.
const REPLAYED: usize = 40;

/// The entry `svnserve` hides from the anonymous principal in a listing (`make-fixtures.py`'s
/// `AUTHZ`), which A-613's `get-dir` lists: the read commands apply no authorization yet (A-606,
/// owed in `reads`).
const HIDDEN: &[u8] = b"secret";

/// The answers the replay meets where slates lists [`HIDDEN`] and `svnserve` does not: one, the
/// listing of `/trunk` in `update-depth-files`.
const HIDDEN_LISTINGS: usize = 1;

/// The repository `svnserve` named for every recorded session's URL: anonymous reads, users with
/// keys, mergeinfo announced, its UUID its realm (`make-fixtures.py`).
fn fixtures_repository() -> Repository<'static> {
  Repository {
    uuid: UUID,
    root_url: ROOT_URL,
    realm: UUID,
    mergeinfo: true,
    anonymous: true,
    tunnel_user: None,
    cram_md5: true,
  }
}

/// The fixtures' authorization file (`make-fixtures.py` `AUTHZ`): everyone reads, alice writes,
/// and `/trunk/secret` is alice's alone.
const AUTHZ: &[u8] = b"[/]\n* = r\nalice = rw\n[/trunk/secret]\n* =\nalice = rw\n";

/// alice's secret in the fixtures' password database (`make-fixtures.py` `PASSWD`).
const ALICE_SECRET: &[u8] = b"secret";

/// The fixtures' repository.
struct Fixture {
  /// The index.
  index: Index,
  /// The green.
  green: Green,
  /// The lock table: none held.
  locks: LockTable,
  /// The rules of [`AUTHZ`].
  authz: Authz,
}

/// The fixtures' repository built.
fn fixture() -> Fixture {
  let (index, green) = repository();
  Fixture {
    index,
    green,
    locks: LockTable::new(),
    authz: Authz::parse(AUTHZ, None, authz::Bounds::of(AUTHZ, None)).unwrap(),
  }
}

impl Fixture {
  /// What every call of a session established on the fixtures' repository hands over, the guards
  /// `guards` (`make-fixtures.py` `SVNSERVE_CONF`: anonymous users read, authenticated ones write,
  /// under [`AUTHZ`]).
  fn sources<'f>(&'f self, guards: &'f FixtureGuards) -> Sources<'f, Green, FixtureGuards> {
    Sources {
      index: &self.index,
      repository: &self.green,
      guards,
      locks: &self.locks,
      policy: Policy {
        anonymous: authz::Access::Read,
        authenticated: authz::Access::Write,
        authz: Some((&self.authz, b"repo")),
        filesystem: "repo",
      },
    }
  }
}

/// How a replay ended.
#[derive(Debug, PartialEq, Eq)]
enum Ending {
  /// The client's bytes ran out.
  Waiting,
  /// The connection refused them.
  Refused(Refused),
  /// A command not served yet was answered, its answer at this item of the output.
  Unserved(usize),
}

/// Replays a recorded session's client bytes through a connection, an item at a time, each handed
/// over when the connection asks for more (with the item that answered a drive early when the
/// session's client sent it so).
fn replay(fixture: &Fixture, recorded: Recorded, budget: Budget) -> (Vec<u8>, Ending) {
  let guards = session_guards(recorded);
  let sources = fixture.sources(&guards);
  let client = items(recorded.client);
  let mut out = Vec::new();
  let mut connection = Connection::new(LIMITS, &mut out).unwrap();
  let mut next = client.iter();
  let mut held: Vec<u8> = Vec::new();
  let mut established = false;
  loop {
    let given = established.then_some(&sources);
    let before = items(&out).len();
    match connection.receive(given, &held, &mut out, budget) {
      Ok(Turn::NeedMore { .. }) => match next.next() {
        Some(item) => deliver(recorded, item, &mut next, &mut held),
        None => return (out, Ending::Waiting),
      },
      Ok(Turn::Took { consumed }) => {
        held.drain(..consumed);
      }
      Ok(Turn::Wrote) => {}
      Ok(Turn::Opened { consumed, .. }) => {
        held.drain(..consumed);
        assert_eq!(
          connection.open(&fixtures_repository(), &mut out),
          Ok(Access::Asked)
        );
      }
      Ok(Turn::Established { consumed, .. }) => {
        held.drain(..consumed);
        established = true;
      }
      Ok(Turn::Unserved { .. }) => return (out, Ending::Unserved(before)),
      Ok(other) => panic!("{}: {other:?}", recorded.name),
      Err(refused) => return (out, Ending::Refused(refused)),
    }
  }
}

/// Hands over one client item, and with `finish-report` the next one too when the session's client
/// sent its answer early.
fn deliver<'i>(
  recorded: Recorded,
  item: &[u8],
  next: &mut impl Iterator<Item = &'i &'i [u8]>,
  held: &mut Vec<u8>,
) {
  held.extend_from_slice(item);
  let early = recorded.name == "hostile-report-early-edit-response.1";
  if early && command(item).is_some_and(|(name, _)| name == "finish-report") {
    held.extend_from_slice(next.next().unwrap());
  }
}

/// The guards a session ran under: the anonymous principal's, and the lock its report names where
/// it held one.
fn session_guards(recorded: Recorded) -> FixtureGuards {
  let reports = items(recorded.client)
    .iter()
    .any(|item| command(item).is_some_and(|(name, _)| RequestKind::of(&name).is_some()));
  if reports && recorded.name == "update-with-lock.1" {
    guards_of(recorded.name, &finished_report(recorded))
  } else {
    FixtureGuards::anonymous()
  }
}

/// An item as a tree, every list of records led by a name sorted, so property lists and a
/// directory's entries compare as sets.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Tree {
  /// A word.
  Word(String),
  /// A number.
  Number(u64),
  /// A string.
  Bytes(Vec<u8>),
  /// A list.
  List(Vec<Tree>),
}

/// The tree of an item's value.
fn tree(value: &Value<'_>) -> Tree {
  match value {
    Value::Word(word) => Tree::Word((*word).to_owned()),
    Value::Number(number) => Tree::Number(*number),
    Value::String(bytes) => Tree::Bytes(bytes.to_vec()),
    Value::List(items) => {
      let mut trees: Vec<Tree> = items.iter().map(tree).collect();
      let named = |tree: &Tree| matches!(tree, Tree::List(record) if record.len() >= 2 && matches!(record[0], Tree::Bytes(_)));
      if !trees.is_empty() && trees.iter().all(named) {
        trees.sort();
      }
      Tree::List(trees)
    }
  }
}

/// The span of `items` that is a drive: from its first editor command while they are editor
/// commands; empty at the end when there is none.
fn drive_span(items: &[&[u8]]) -> (usize, usize) {
  let joined: Vec<u8> = items.concat();
  let drive = drive_items(&joined);
  let start = items
    .iter()
    .position(|item| drive.first() == Some(item))
    .unwrap_or(items.len());
  (start, start + drive.len())
}

/// A tree without the directory entry named [`HIDDEN`].
fn without_hidden(tree: Tree) -> Tree {
  match tree {
    Tree::List(items) => Tree::List(
      items
        .into_iter()
        .filter(|item| !matches!(item, Tree::List(record) if record.first() == Some(&Tree::Bytes(HIDDEN.to_vec())) && matches!(record.get(1), Some(Tree::Word(_)))))
        .map(without_hidden)
        .collect(),
    ),
    other => other,
  }
}

/// Asserts that items match one for one as trees, ours allowed to list [`HIDDEN`] where theirs
/// does not; the answers that did so are counted into `hidden`.
fn assert_items(name: &str, ours: &[&[u8]], theirs: &[&[u8]], hidden: &mut usize) {
  assert_eq!(ours.len(), theirs.len(), "{name}: the items");
  for (index, (ours, theirs)) in ours.iter().zip(theirs).enumerate() {
    let (ours, theirs) = (tree(&value_of(ours)), tree(&value_of(theirs)));
    if ours != theirs {
      assert_eq!(without_hidden(ours), theirs, "{name}: item {index}");
      *hidden += 1;
    }
  }
}

/// Asserts that a connection wrote what `svnserve` wrote in its session, up to `end` of our items:
/// the greeting less `svndiff1`, the items before the drive and after it as trees, the drive call
/// for call in the shared form.
fn assert_as_svnserve(
  name: &str,
  ours: &[&[u8]],
  server: &[u8],
  texts: &BTreeMap<[u8; 16], Vec<u8>>,
  hidden: &mut usize,
) {
  let theirs = items(server);
  let greeting: Vec<u8> = {
    let greeting = theirs[0];
    let at = greeting
      .windows(SVNDIFF1.len())
      .position(|window| window == SVNDIFF1)
      .unwrap();
    [&greeting[..at], &greeting[at + SVNDIFF1.len()..]].concat()
  };
  assert_eq!(ours[0], greeting.as_slice(), "{name}: the greeting");
  let (our_start, our_end) = drive_span(ours);
  let (their_start, their_end) = drive_span(&theirs);
  if our_start == ours.len() {
    assert_items(name, &ours[1..], &theirs[1..ours.len()], hidden);
    return;
  }
  assert_items(name, &ours[1..our_start], &theirs[1..their_start], hidden);
  let our_drive = read_drive(&ours[our_start..our_end]).calls;
  let their_drive = read_drive(&theirs[their_start..their_end]).calls;
  assert_eq!(
    normalize(our_drive, texts),
    normalize(their_drive, texts),
    "{name}: the drive"
  );
  let after = ours.len() - our_end;
  assert_items(
    name,
    &ours[our_end..],
    &theirs[their_end..their_end + after],
    hidden,
  );
}

/// A-656 (`serve.c`'s command loop against stock `svnserve` 1.15.0): replay every recorded session
/// from its first byte through a connection over the fixtures' repository, the client's items
/// handed over as a stock client sends them; expect `svnserve`'s greeting less `svndiff1`, every
/// item as a tree, the drive call for call, up to a command not served yet, answered "Unknown editor
/// command"; and every recorded session met, the listing that shows an entry `svnserve` hides the
/// one place they differ.
#[test]
fn every_recorded_session_is_answered_as_svnserve_answered_it() {
  let fixture = fixture();
  let texts = texts_by_md5(&fixture.green);
  let (mut replayed, mut hidden) = (0, 0);
  for recorded in SESSIONS.iter().copied() {
    let (out, ending) = replay(&fixture, recorded, PLENTY);
    replayed += 1;
    let ours = items(&out);
    let compared = match ending {
      Ending::Unserved(at) => {
        let (name, _) = command(ours[at]).unwrap();
        assert_eq!(name, "failure", "{}: the unserved answer", recorded.name);
        at
      }
      Ending::Refused(refused) => {
        // svnserve ended this connection before it flushed the trivial auth request it had
        // answered `update` with (A-616 part 1): only what it flushed is compared.
        assert_eq!(
          recorded.name, "hostile-report-malformed-set-path.1",
          "{refused:?}"
        );
        ours.len().min(items(recorded.server).len())
      }
      Ending::Waiting => ours.len(),
    };
    assert_as_svnserve(
      recorded.name,
      &ours[..compared],
      recorded.server,
      &texts,
      &mut hidden,
    );
  }
  assert_eq!(replayed, REPLAYED);
  assert_eq!(hidden, HIDDEN_LISTINGS);
}

/// A-656 (bounded turns): replay the recorded checkout and an update with one byte and one step a
/// call; expect what the whole budget wrote.
#[test]
fn a_connection_writes_alike_under_the_smallest_budget() {
  let fixture = fixture();
  for name in ["checkout-r1.1", "update-to-head.1", "diff-r1-r6.2"] {
    let recorded = SESSIONS
      .iter()
      .copied()
      .find(|session| session.name == name)
      .unwrap();
    assert_eq!(
      replay(&fixture, recorded, SMALLEST),
      replay(&fixture, recorded, PLENTY),
      "{name}"
    );
  }
}

/// How a stock client authenticates anonymously.
const ANONYMOUSLY: &str = "( ANONYMOUS ( 0: ) ) ";

/// How a stock client authenticates through a tunnel.
const EXTERNALLY: &str = "( EXTERNAL ( 0: ) ) ";

/// A stock 1.15 client's answer to the greeting for `url`, and its authentication `auth`.
fn opening(url: &str, auth: &str) -> Vec<u8> {
  format!(
    "( 2 ( edit-pipeline svndiff1 accepts-svndiff2 absent-entries depth mergeinfo log-revprops ) \
     {}:{url} 10:SVN/1.15.0 ( ) ) {auth}",
    url.len()
  )
  .into_bytes()
}

/// A connection established on the fixtures' repository at `/trunk`, anonymously.
fn established() -> Connection {
  established_on(&fixtures_repository(), ANONYMOUSLY)
}

/// A connection established on `repository` at `/trunk`, the client authenticating with `auth`,
/// what it wrote so far dropped.
fn established_on(repository: &Repository<'_>, auth: &str) -> Connection {
  let mut out = Vec::new();
  let mut connection = Connection::new(LIMITS, &mut out).unwrap();
  let opening = opening("svn://127.0.0.1/repo/trunk", auth);
  let mut rest = opening.as_slice();
  while connection.session().is_none() {
    let none = None::<&Sources<'_, Green, FixtureGuards>>;
    match connection.receive(none, rest, &mut out, PLENTY).unwrap() {
      Turn::Opened { consumed, .. } => {
        rest = &rest[consumed..];
        connection.open(repository, &mut out).unwrap();
      }
      Turn::Took { consumed } | Turn::Established { consumed, .. } => rest = &rest[consumed..],
      other => panic!("{other:?}"),
    }
  }
  connection
}

/// Sends `command` on an established connection and returns what it wrote and how it ended.
fn send(
  fixture: &Fixture,
  guards: &FixtureGuards,
  connection: &mut Connection,
  command: &[u8],
) -> (String, Result<String, Refused>) {
  let sources = fixture.sources(guards);
  let mut out = Vec::new();
  let turn = connection.receive(Some(&sources), command, &mut out, PLENTY);
  let turn = turn.map(|turn| format!("{turn:?}"));
  (String::from_utf8_lossy(&out).into_owned(), turn)
}

/// A-656 (`serve.c` `reparent`): reparent an established session from `/trunk` to `/tags`, then
/// check `a.txt` there; reparent it outside the repository; send a `reparent` whose URL is no
/// string; expect a trivial auth request and a success, no `a.txt` beneath `/tags`, then a trivial
/// auth request and 170000 "'…' is not the same repository as '…'", then the connection's end.
#[test]
fn reparent_moves_the_session_within_its_repository() {
  let fixture = fixture();
  let guards = FixtureGuards::anonymous();
  let mut connection = established();
  let (out, turn) = send(
    &fixture,
    &guards,
    &mut connection,
    b"( reparent ( 25:svn://127.0.0.1/repo/tags ) ) ",
  );
  assert_eq!(out, "( success ( ( ) 0: ) ) ( success ( ) ) ");
  assert!(turn.unwrap().starts_with("Took"));
  let (out, _) = send(
    &fixture,
    &guards,
    &mut connection,
    b"( check-path ( 5:a.txt ( 6 ) ) ) ",
  );
  assert!(out.ends_with("( success ( none ) ) "), "{out}");
  let (out, _) = send(
    &fixture,
    &guards,
    &mut connection,
    b"( reparent ( 23:svn://example.org/other ) ) ",
  );
  let message = "'svn://example.org/other' is not the same repository as 'svn://127.0.0.1/repo'";
  let failure = format!(
    "( failure ( ( 170000 {}:{message} 0: 0 ) ) ) ",
    message.len()
  );
  assert_eq!(out, format!("( success ( ( ) 0: ) ) {failure}"));
  let (out, turn) = send(&fixture, &guards, &mut connection, b"( reparent ( 5 ) ) ");
  assert_eq!((out.as_str(), turn), ("", Err(Refused::Malformed)));
}

/// A-656 (commands not served yet, and items that are no command): send `get-file-revs`, then
/// `get-latest-rev`; then an item that is a number; expect "Unknown editor command
/// 'get-file-revs'" (210001) and the connection going on, the youngest revision answered, then the
/// connection's end.
#[test]
fn a_command_not_served_yet_is_answered_and_the_session_goes_on() {
  let fixture = fixture();
  let guards = FixtureGuards::anonymous();
  let mut connection = established();
  let item = b"( get-file-revs ( 5:a.txt ( 1 ) ( 6 ) ) ) ";
  let (out, turn) = send(&fixture, &guards, &mut connection, item);
  let message = "Unknown editor command 'get-file-revs'";
  assert_eq!(
    out,
    format!(
      "( failure ( ( 210001 {}:{message} 0: 0 ) ) ) ",
      message.len()
    )
  );
  let unserved = Turn::Unserved {
    command: "get-file-revs",
    consumed: item.len(),
  };
  assert_eq!(turn, Ok(format!("{unserved:?}")));
  let (out, _) = send(
    &fixture,
    &guards,
    &mut connection,
    b"( get-latest-rev ( ) ) ",
  );
  assert!(out.ends_with("( success ( 6 ) ) "), "{out}");
  let (out, turn) = send(&fixture, &guards, &mut connection, b"5 ");
  assert_eq!((out.as_str(), turn), ("", Err(Refused::Malformed)));
}

/// A-656 (the caller's errors): call an established connection without the repository's sources;
/// call one that has ended; expect each refused as the caller's, nothing written.
#[test]
fn a_call_without_sources_or_after_the_end_is_the_callers_error() {
  let fixture = fixture();
  let guards = FixtureGuards::anonymous();
  let mut connection = established();
  let mut out = Vec::new();
  let none = None::<&Sources<'_, Green, FixtureGuards>>;
  let refused = connection.receive(none, b"( get-latest-rev ( ) ) ", &mut out, PLENTY);
  assert_eq!(refused, Err(Refused::NoSources));
  assert_eq!(
    connection.receive(none, b"", &mut out, PLENTY),
    Err(Refused::OutOfTurn)
  );
  assert!(out.is_empty());
}

/// A-656 (what the connection waits for): walk the recorded checkout one call at a time; expect
/// the handshake, then a command, the report, the drive written, the edit's answer, and a command
/// again, in that order.
#[test]
fn a_connection_names_what_it_waits_for() {
  let fixture = fixture();
  let recorded = SESSIONS
    .iter()
    .copied()
    .find(|session| session.name == "checkout-r1.1")
    .unwrap();
  let guards = FixtureGuards::anonymous();
  let sources = fixture.sources(&guards);
  let client = items(recorded.client);
  let mut out = Vec::new();
  let mut connection = Connection::new(LIMITS, &mut out).unwrap();
  let mut seen = vec![connection.waiting()];
  let (mut next, mut held, mut established) = (client.iter(), Vec::new(), false);
  loop {
    let given = established.then_some(&sources);
    match connection
      .receive(given, &held, &mut out, SMALLEST)
      .unwrap()
    {
      Turn::NeedMore { .. } => match next.next() {
        Some(item) => held.extend_from_slice(item),
        None => break,
      },
      Turn::Took { consumed } => drop(held.drain(..consumed)),
      Turn::Opened { consumed, .. } => {
        held.drain(..consumed);
        connection.open(&fixtures_repository(), &mut out).unwrap();
      }
      Turn::Established { consumed, .. } => {
        held.drain(..consumed);
        established = true;
      }
      _ => {}
    }
    if seen.last() != Some(&connection.waiting()) {
      seen.push(connection.waiting());
    }
  }
  let order = [
    Waiting::Handshake,
    Waiting::Command,
    Waiting::Report,
    Waiting::Writing,
    Waiting::EditAnswer,
    Waiting::Command,
  ];
  assert_eq!(seen, order);
}

/// A-656 (one connection, two transports): open a repository whose only user is the tunnel's,
/// `alice`, and answer `EXTERNAL`; open one that admits no one; refuse a URL; expect alice
/// established, then "No access allowed to this repository" and the end, then the refusal and the
/// end.
#[test]
fn a_tunnels_user_is_taken_and_a_closed_repository_ends_the_connection() {
  let tunnel = Repository {
    anonymous: false,
    tunnel_user: Some(b"alice"),
    cram_md5: false,
    ..fixtures_repository()
  };
  let mut out = Vec::new();
  let mut connection = Connection::new(LIMITS, &mut out).unwrap();
  let answer = b"( 2 ( edit-pipeline ) 26:svn://127.0.0.1/repo/trunk ) ";
  let none = None::<&Sources<'_, Green, FixtureGuards>>;
  let opened = connection.receive(none, answer, &mut out, PLENTY).unwrap();
  assert!(matches!(opened, Turn::Opened { .. }));
  assert_eq!(connection.open(&tunnel, &mut out), Ok(Access::Asked));
  let external = connection.receive(none, b"( EXTERNAL ( 0: ) ) ", &mut out, PLENTY);
  let alice = Principal::User(b"alice".to_vec());
  assert!(matches!(external, Ok(Turn::Established { principal, .. }) if principal == alice));
  assert_eq!(connection.waiting(), Waiting::Command);
  let closed = Repository {
    anonymous: false,
    cram_md5: false,
    ..fixtures_repository()
  };
  let mut shut = Connection::new(LIMITS, &mut out).unwrap();
  let _ = shut.receive(none, answer, &mut out, PLENTY).unwrap();
  assert_eq!(shut.open(&closed, &mut out), Ok(Access::Denied));
  assert_eq!(shut.waiting(), Waiting::Ended);
  let mut refused = Connection::new(LIMITS, &mut out).unwrap();
  let _ = refused.receive(none, answer, &mut out, PLENTY).unwrap();
  assert_eq!(refused.refuse(Unserved::BadPath, &mut out), Ok(()));
  assert_eq!(refused.waiting(), Waiting::Ended);
}

/// The challenge the tests' CRAM-MD5 attempts are given: its random number.
const NONCE: u64 = 5_688_694_775_020_434_939;

/// The challenge's instant, in microseconds.
const TIME_MICROS: i64 = 1_795_059_982_203_537;

/// The challenge's host.
const HOST: &CStr = c"slates.test";

/// The date the tests' made revisions are answered with.
const DATE: &str = "2026-11-19T05:05:32.129962Z";

/// An update of `secret` beneath `/trunk`, which only alice may read (`make-fixtures.py` `AUTHZ`).
const UPDATE_SECRET: &[u8] = b"( update ( ( 6 ) 6:secret true infinity false false ) ) ";

/// A commit with its log message and no lock tokens, as a stock client sends one.
const COMMIT_NEWDIR: &[u8] = b"( commit ( 6:newdir ( ) false ( ( 7:svn:log 6:newdir ) ) ) ) ";

/// A drive adding the directory `newdir` beneath the session's path, up to its `close-edit`.
const ADD_NEWDIR: [&str; 4] = [
  "( open-root ( ( ) 2:d0 ) ) ",
  "( add-dir ( 6:newdir 2:d0 2:d1 ( ) ) ) ",
  "( close-dir ( 2:d1 ) ) ",
  "( close-dir ( 2:d0 ) ) ",
];

/// The command that ends a drive.
const CLOSE_EDIT: &[u8] = b"( close-edit ( ) ) ";

/// The request for an authentication for one command where the fixtures' users hold keys:
/// `CRAM-MD5` alone, in the repository's realm, its UUID.
fn cram_md5_request() -> String {
  let realm = std::str::from_utf8(UUID).unwrap();
  format!("( success ( ( CRAM-MD5 ) {}:{realm} ) ) ", realm.len())
}

/// "Authorization failed" (170001) as `svnserve` writes it.
fn not_authorized() -> String {
  let message = "Authorization failed";
  format!(
    "( failure ( ( 170001 {}:{message} 0: 0 ) ) ) ",
    message.len()
  )
}

/// The tunnel through which alice reaches the fixtures' repository: her user `EXTERNAL` takes, no
/// anonymous access and no keys.
fn alices_tunnel() -> Repository<'static> {
  Repository {
    anonymous: false,
    tunnel_user: Some(b"alice"),
    cram_md5: false,
    ..fixtures_repository()
  }
}

/// The transaction the tests' commits are begun as.
const TRANSACTION: &str = "6-1";

/// What the tests begin a commit with: [`TRANSACTION`], at the epoch, the report's room.
fn start() -> CommitStart {
  CommitStart {
    transaction: TRANSACTION.to_owned(),
    now: AprTime(0),
    room: LIMITS.report_room,
  }
}

/// Answers the authentication the connection asked for with CRAM-MD5 as `user`, whose key the
/// caller holds, made from `secret`: what the connection wrote, and its verdict.
fn cram_md5(connection: &mut Connection, user: &[u8], secret: &[u8]) -> (String, Verdict) {
  let none = None::<&Sources<'_, Green, FixtureGuards>>;
  let mut out = Vec::new();
  let choice = b"( CRAM-MD5 ( ) ) ";
  let chose = connection.receive(none, choice, &mut out, PLENTY);
  let challenge_needed = Turn::ChallengeNeeded {
    consumed: choice.len(),
  };
  assert_eq!(chose, Ok(challenge_needed));
  connection
    .challenge(NONCE, TIME_MICROS, HOST, &mut out)
    .unwrap();
  let key = CramKey::from_secret(secret).unwrap();
  let challenge = format!("<{NONCE}.{TIME_MICROS}@{}>", HOST.to_str().unwrap());
  let digest: String = key
    .digest(challenge.as_bytes())
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect();
  let text = [user, b" ".as_slice(), digest.as_bytes()].concat();
  let length = format!("{}:", text.len());
  let response = [length.as_bytes(), text.as_slice(), b" ".as_slice()].concat();
  let named = connection.receive(none, &response, &mut out, PLENTY);
  let key_needed = Turn::KeyNeeded {
    user,
    consumed: response.len(),
  };
  assert_eq!(named, Ok(key_needed));
  let verdict = connection.key(Some(&key), &mut out).unwrap();
  (String::from_utf8_lossy(&out).into_owned(), verdict)
}

/// Drives [`ADD_NEWDIR`] and `close-edit` through `connection`, each command taken without a word,
/// start-commit answered at `open-root` and pre-commit at `close-edit`, both passing; returns the
/// work accepted once pre-commit passed, for which nothing is written.
fn drive_newdir(
  fixture: &Fixture,
  guards: &FixtureGuards,
  connection: &mut Connection,
) -> Box<Work> {
  let sources = fixture.sources(guards);
  for (at, command) in ADD_NEWDIR.into_iter().enumerate() {
    let mut out = Vec::new();
    let turn = connection.receive(Some(&sources), command.as_bytes(), &mut out, PLENTY);
    if at == 0 {
      let due = Turn::Hook {
        due: Due::StartCommit {
          transaction: TRANSACTION.to_owned(),
        },
        consumed: command.len(),
      };
      assert_eq!(turn, Ok(due));
      let answered = connection.answer_hook(Ok(()), &sources, &mut out);
      assert_eq!(answered, Ok(Turn::Took { consumed: 0 }));
    } else {
      let took = Turn::Took {
        consumed: command.len(),
      };
      assert_eq!(turn, Ok(took));
    }
    assert!(out.is_empty(), "{command}: {out:?}");
    assert_eq!(connection.waiting(), Waiting::Edit);
  }
  let mut out = Vec::new();
  let closing = connection.receive(Some(&sources), CLOSE_EDIT, &mut out, PLENTY);
  let due = Turn::Hook {
    due: Due::PreCommit {
      transaction: TRANSACTION.to_owned(),
    },
    consumed: CLOSE_EDIT.len(),
  };
  assert_eq!(closing, Ok(due));
  assert_eq!(connection.waiting(), Waiting::Writing);
  let work = match connection.answer_hook(Ok(()), &sources, &mut out) {
    Ok(Turn::Accepted { work, consumed: 0 }) => work,
    other => panic!("{other:?}"),
  };
  assert!(out.is_empty(), "{out:?}");
  assert_eq!(connection.waiting(), Waiting::Writing);
  work
}

/// A-656 (`serve.c` `update`'s `must_have_access` on the session's path joined to the target,
/// 807-850): ask anonymously to update `secret` beneath `/trunk`, which only alice may read, where
/// users hold keys; authenticate as alice by CRAM-MD5; call again with her guards; expect the
/// request listing `CRAM-MD5` alone in the repository's realm, the challenge, `( success ( ) )`
/// alone, then the report awaited with nothing written, no trivial auth request after a real one,
/// and the session alice's.
#[test]
fn an_update_of_a_target_the_principal_may_not_read_authenticates_first() {
  let fixture = fixture();
  let anonymous = FixtureGuards::anonymous();
  let mut connection = established();
  let (out, turn) = send(&fixture, &anonymous, &mut connection, UPDATE_SECRET);
  assert_eq!(out, cram_md5_request());
  assert!(turn.unwrap().starts_with("Took"));
  assert_eq!(connection.waiting(), Waiting::Handshake);
  let (out, verdict) = cram_md5(&mut connection, b"alice", ALICE_SECRET);
  let alice = Principal::User(b"alice".to_vec());
  assert_eq!(verdict, Verdict::Established(alice.clone()));
  assert!(out.ends_with("> ) ) ( success ( ) ) "), "{out}");
  assert_eq!(connection.waiting(), Waiting::Writing);
  let (out, turn) = send(&fixture, &FixtureGuards::default(), &mut connection, b"");
  assert_eq!((out.as_str(), turn), ("", Ok(format!("{:?}", Turn::Wrote))));
  assert_eq!(connection.waiting(), Waiting::Report);
  let principal = connection.session().map(|session| &session.principal);
  assert_eq!(principal, Some(&alice));
}

/// A-656 (`must_have_access` 826-844 where authentication cannot help, and a readable target):
/// ask anonymously to update `secret` on a repository that holds no keys, then `dir`; then, where
/// keys are held, authenticate as bob, whom the rules keep out of `secret`, and call again with his
/// guards; expect "Authorization failed" (170001) with no request before it and the next command
/// awaited, then `dir`'s trivial auth request and its report awaited; then the request, bob's
/// success, and "Authorization failed" with no trivial auth request before it.
#[test]
fn an_update_the_principal_may_not_read_is_refused_where_authentication_cannot_help() {
  let fixture = fixture();
  let anonymous = FixtureGuards::anonymous();
  let keyless = Repository {
    cram_md5: false,
    ..fixtures_repository()
  };
  let mut connection = established_on(&keyless, ANONYMOUSLY);
  let (out, _) = send(&fixture, &anonymous, &mut connection, UPDATE_SECRET);
  assert_eq!(out, not_authorized());
  assert_eq!(connection.waiting(), Waiting::Command);
  let update_dir = b"( update ( ( 6 ) 3:dir true infinity false false ) ) ";
  let (out, _) = send(&fixture, &anonymous, &mut connection, update_dir);
  assert_eq!(out, "( success ( ( ) 0: ) ) ");
  assert_eq!(connection.waiting(), Waiting::Report);
  let mut connection = established();
  let (out, _) = send(&fixture, &anonymous, &mut connection, UPDATE_SECRET);
  assert_eq!(out, cram_md5_request());
  let (_, verdict) = cram_md5(&mut connection, b"bob", b"bob's secret");
  let bob = Principal::User(b"bob".to_vec());
  assert_eq!(verdict, Verdict::Established(bob));
  let (out, turn) = send(&fixture, &anonymous, &mut connection, b"");
  assert_eq!(
    (out, turn),
    (not_authorized(), Ok(format!("{:?}", Turn::Wrote)))
  );
  assert_eq!(connection.waiting(), Waiting::Command);
}

/// A-656 with A-655 (`serve.c` `commit` 1471-1580): commit, through a tunnel as alice, a drive
/// adding `newdir` beneath `/trunk`; answer the work as made into revision 7; expect the commit
/// handed to the caller with nothing written, then the trivial auth request and the commit's
/// success, the drive's commands taken without a word, the work accepted at `close-edit` with
/// nothing written (one row, `/trunk/newdir` added, `svn:author` alice's), then `close-edit`'s
/// success, the trivial auth request and the new revision with its date and author, and the next
/// command awaited.
#[test]
fn a_commit_is_driven_and_answered_with_the_revision_the_owner_made() {
  let fixture = fixture();
  let alice = FixtureGuards::default();
  let mut connection = established_on(&alices_tunnel(), EXTERNALLY);
  let (out, turn) = send(&fixture, &alice, &mut connection, COMMIT_NEWDIR);
  let asked = Turn::CommitAsked {
    consumed: COMMIT_NEWDIR.len(),
  };
  assert_eq!((out.as_str(), turn), ("", Ok(format!("{asked:?}"))));
  assert_eq!(connection.waiting(), Waiting::Writing);
  let mut out = Vec::new();
  connection
    .commit(start(), &fixture.sources(&alice), &mut out)
    .unwrap();
  let begun = "( success ( ( ) 0: ) ) ( success ( ) ) ";
  assert_eq!(String::from_utf8_lossy(&out), begun);
  let work = drive_newdir(&fixture, &alice, &mut connection);
  let rows: Vec<(&str, Action)> = work
    .record
    .changes
    .iter()
    .map(|row| (row.path.as_str(), row.action))
    .collect();
  assert_eq!(rows, [("/trunk/newdir", Action::Add)]);
  let author = (b"svn:author".to_vec(), b"alice".to_vec());
  assert!(work.revision_properties.contains(&author));
  let mut out = Vec::new();
  let made = Made {
    revision: 7,
    date: DATE,
    author: Some("alice"),
    post_commit_error: None,
  };
  connection.committed(&made, &mut out).unwrap();
  let answer = format!(
    "( success ( ) ) ( success ( ( ) 0: ) ) ( 7 ( {}:{DATE} ) ( 5:alice ) ( ) ) ",
    DATE.len()
  );
  assert_eq!(String::from_utf8_lossy(&out), answer);
  assert_eq!(connection.waiting(), Waiting::Command);
}

/// A-656 with A-655 (`must_have_access` 826-836 for `commit`, then `editorp.c` 1149-1208): commit
/// anonymously where users hold keys; authenticate as alice by CRAM-MD5; call again with her
/// guards; drive `newdir`; refuse the accepted work as the owner refuses every one until A-655
/// part 2; send a stray editor command, a number, then `abort-edit`; ask for the youngest
/// revision; expect the request listing `CRAM-MD5` alone, the success alone, then the commit's
/// success with no trivial auth request before it, the work accepted, then 200007 "This server
/// does not make commits yet" as `close-edit`'s failure, the stray items dropped without a word,
/// `abort-edit` ending the edit without one, and the youngest revision answered to alice's session.
#[test]
fn an_anonymous_commit_authenticates_for_write_and_an_unmade_work_is_refused() {
  let fixture = fixture();
  let anonymous = FixtureGuards::anonymous();
  let mut connection = established();
  let (_, turn) = send(&fixture, &anonymous, &mut connection, COMMIT_NEWDIR);
  assert!(turn.unwrap().starts_with("CommitAsked"));
  let mut out = Vec::new();
  connection
    .commit(start(), &fixture.sources(&anonymous), &mut out)
    .unwrap();
  assert_eq!(String::from_utf8_lossy(&out), cram_md5_request());
  assert_eq!(connection.waiting(), Waiting::Handshake);
  let (out, verdict) = cram_md5(&mut connection, b"alice", ALICE_SECRET);
  let alice_principal = Principal::User(b"alice".to_vec());
  assert_eq!(verdict, Verdict::Established(alice_principal.clone()));
  assert!(out.ends_with("> ) ) ( success ( ) ) "), "{out}");
  let alice = FixtureGuards::default();
  let (out, turn) = send(&fixture, &alice, &mut connection, b"");
  let wrote = Ok(format!("{:?}", Turn::Wrote));
  assert_eq!((out.as_str(), turn), ("( success ( ) ) ", wrote));
  assert_eq!(connection.waiting(), Waiting::Edit);
  drive_newdir(&fixture, &alice, &mut connection);
  let mut out = Vec::new();
  connection
    .not_committed(&Uncommitted::NotServed, &mut out)
    .unwrap();
  let failure = format!(
    "( failure ( ( 200007 {}:{NOT_SERVED} 0: 0 ) ) ) ",
    NOT_SERVED.len()
  );
  assert_eq!(String::from_utf8_lossy(&out), failure);
  assert_eq!(connection.waiting(), Waiting::Edit);
  for stray in ["( close-dir ( 2:d0 ) ) ", "5 "] {
    let (out, turn) = send(&fixture, &alice, &mut connection, stray.as_bytes());
    let took = Turn::Took {
      consumed: stray.len(),
    };
    assert_eq!((out.as_str(), turn), ("", Ok(format!("{took:?}"))));
    assert_eq!(connection.waiting(), Waiting::Edit);
  }
  let (out, _) = send(&fixture, &alice, &mut connection, b"( abort-edit ( ) ) ");
  assert_eq!(out, "");
  assert_eq!(connection.waiting(), Waiting::Command);
  let (out, _) = send(
    &fixture,
    &alice,
    &mut connection,
    b"( get-latest-rev ( ) ) ",
  );
  assert!(out.ends_with("( success ( 6 ) ) "), "{out}");
  let principal = connection.session().map(|session| &session.principal);
  assert_eq!(principal, Some(&alice_principal));
}

/// A-656 with A-655 (`must_have_access` 840-844 for `commit`): commit anonymously where users hold
/// keys; authenticate as bob, whom the rules let read alone; call again with his guards; expect the
/// request, bob's success, then "Authorization failed" with no trivial auth request before it, and
/// the next command awaited.
#[test]
fn a_commit_by_a_user_the_rules_keep_from_writing_is_refused_after_the_authentication() {
  let fixture = fixture();
  let anonymous = FixtureGuards::anonymous();
  let mut connection = established();
  send(&fixture, &anonymous, &mut connection, COMMIT_NEWDIR)
    .1
    .unwrap();
  let mut out = Vec::new();
  connection
    .commit(start(), &fixture.sources(&anonymous), &mut out)
    .unwrap();
  assert_eq!(String::from_utf8_lossy(&out), cram_md5_request());
  let (_, verdict) = cram_md5(&mut connection, b"bob", b"bob's secret");
  let bob = Principal::User(b"bob".to_vec());
  assert_eq!(verdict, Verdict::Established(bob));
  let (out, turn) = send(&fixture, &anonymous, &mut connection, b"");
  assert_eq!(
    (out, turn),
    (not_authorized(), Ok(format!("{:?}", Turn::Wrote)))
  );
  assert_eq!(connection.waiting(), Waiting::Command);
}

/// A-656 with A-655 (`transaction.c` `commit_body` 3784-3786 through A-655's step): refuse an
/// accepted work as the owner's commit step refuses one whose head a commit passed since the
/// verdict; expect `close-edit`'s failure in the step's words, 160028 "Transaction out of date",
/// and the client's commands awaited until its `abort-edit`.
#[test]
fn a_work_the_owners_step_refuses_is_answered_in_its_words() {
  let fixture = fixture();
  let alice = FixtureGuards::default();
  let mut connection = established_on(&alices_tunnel(), EXTERNALLY);
  send(&fixture, &alice, &mut connection, COMMIT_NEWDIR)
    .1
    .unwrap();
  connection
    .commit(start(), &fixture.sources(&alice), &mut Vec::new())
    .unwrap();
  let work = drive_newdir(&fixture, &alice, &mut connection);
  let moved = StepRefusal::OutOfDate {
    head: work.head,
    youngest: work.head + 1,
  };
  let mut out = Vec::new();
  connection
    .not_committed(&Uncommitted::Step(moved), &mut out)
    .unwrap();
  let message = "Transaction out of date";
  let failure = format!(
    "( failure ( ( 160028 {}:{message} 0: 0 ) ) ) ",
    message.len()
  );
  assert_eq!(String::from_utf8_lossy(&out), failure);
  assert_eq!(connection.waiting(), Waiting::Edit);
}

/// A-656 with A-661 (start-commit refusing, `fs-wrap.c` `svn_repos_fs_begin_txn_for_commit2` run
/// inside `open-root`): commit through a tunnel as alice; look at the transaction the hook would
/// read; answer start-commit with a failure of two errors; send the drive's next command, a number
/// and `abort-edit`; expect the transaction named `6-1` on the youngest revision with alice's log
/// message, the failure written as `open-root`'s, both errors outermost first, the commands after
/// it dropped without a word, `abort-edit` ending the edit, and the next command awaited.
#[test]
fn a_start_commit_hooks_failure_is_open_roots_and_the_drive_is_dropped() {
  let fixture = fixture();
  let alice = FixtureGuards::default();
  let sources = fixture.sources(&alice);
  let mut connection = established_on(&alices_tunnel(), EXTERNALLY);
  send(&fixture, &alice, &mut connection, COMMIT_NEWDIR)
    .1
    .unwrap();
  connection
    .commit(start(), &sources, &mut Vec::new())
    .unwrap();
  let open_root = ADD_NEWDIR[0].as_bytes();
  let mut out = Vec::new();
  let turn = connection.receive(Some(&sources), open_root, &mut out, PLENTY);
  assert!(
    matches!(
      turn,
      Ok(Turn::Hook {
        due: Due::StartCommit { .. },
        ..
      })
    ),
    "{turn:?}"
  );
  let pending = connection.pending().unwrap();
  assert_eq!((pending.transaction, pending.base), (TRANSACTION, 6));
  let log = (b"svn:log".to_vec(), b"newdir".to_vec());
  assert!(pending.revision_properties.contains(&log));
  let refused = vec![
    (
      165_001,
      "Commit blocked by start-commit hook (exit code 1) with output:\nno".to_owned(),
    ),
    (165_001, "the hook's own words".to_owned()),
  ];
  let answered = connection.answer_hook(Err(refused), &sources, &mut out);
  assert_eq!(answered, Ok(Turn::Took { consumed: 0 }));
  let first = "Commit blocked by start-commit hook (exit code 1) with output:\nno";
  let second = "the hook's own words";
  let failure = format!(
    "( failure ( ( 165001 {}:{first} 0: 0 ) ( 165001 {}:{second} 0: 0 ) ) ) ",
    first.len(),
    second.len()
  );
  assert_eq!(String::from_utf8_lossy(&out), failure);
  assert!(connection.pending().is_none());
  for dropped in [ADD_NEWDIR[1], "5 "] {
    let (out, turn) = send(&fixture, &alice, &mut connection, dropped.as_bytes());
    let took = Turn::Took {
      consumed: dropped.len(),
    };
    assert_eq!((out.as_str(), turn), ("", Ok(format!("{took:?}"))));
  }
  let (out, _) = send(&fixture, &alice, &mut connection, b"( abort-edit ( ) ) ");
  assert_eq!(out, "");
  assert_eq!(connection.waiting(), Waiting::Command);
}

/// A-656 with A-661 (pre-commit refusing, `fs-wrap.c` `svn_repos_fs_commit_txn` 69-71 run inside
/// `close-edit` before the verdict): drive `newdir` through a tunnel as alice; answer pre-commit
/// with a failure; expect no work accepted, the failure written as `close-edit`'s, and the edit
/// ended at the client's `abort-edit`.
#[test]
fn a_pre_commit_hooks_failure_is_close_edits_and_no_work_is_accepted() {
  let fixture = fixture();
  let alice = FixtureGuards::default();
  let sources = fixture.sources(&alice);
  let mut connection = established_on(&alices_tunnel(), EXTERNALLY);
  send(&fixture, &alice, &mut connection, COMMIT_NEWDIR)
    .1
    .unwrap();
  connection
    .commit(start(), &sources, &mut Vec::new())
    .unwrap();
  let mut out = Vec::new();
  for (at, command) in ADD_NEWDIR.into_iter().enumerate() {
    connection
      .receive(Some(&sources), command.as_bytes(), &mut out, PLENTY)
      .unwrap();
    if at == 0 {
      connection.answer_hook(Ok(()), &sources, &mut out).unwrap();
    }
  }
  let closing = connection.receive(Some(&sources), CLOSE_EDIT, &mut out, PLENTY);
  assert!(
    matches!(
      closing,
      Ok(Turn::Hook {
        due: Due::PreCommit { .. },
        ..
      })
    ),
    "{closing:?}"
  );
  let refused = vec![(165_001, "no commits on Fridays".to_owned())];
  let answered = connection.answer_hook(Err(refused), &sources, &mut out);
  assert_eq!(answered, Ok(Turn::Took { consumed: 0 }));
  let words = "no commits on Fridays";
  let failure = format!("( failure ( ( 165001 {}:{words} 0: 0 ) ) ) ", words.len());
  assert_eq!(String::from_utf8_lossy(&out), failure);
  assert_eq!(connection.waiting(), Waiting::Edit);
  let (out, _) = send(&fixture, &alice, &mut connection, b"( abort-edit ( ) ) ");
  assert_eq!(out, "");
  assert_eq!(connection.waiting(), Waiting::Command);
}

/// A-656 with A-661 (the caller's errors around a hook): answer a hook where none is due; then,
/// with start-commit due, call `receive`, `commit`, `committed` and `not_committed`; expect each
/// `OutOfTurn` with nothing written and the hook still due, then answered.
#[test]
fn a_call_while_a_hook_is_due_or_an_answer_with_none_due_is_the_callers_error() {
  let fixture = fixture();
  let alice = FixtureGuards::default();
  let sources = fixture.sources(&alice);
  let mut connection = established_on(&alices_tunnel(), EXTERNALLY);
  let mut out = Vec::new();
  let early = connection.answer_hook(Ok(()), &sources, &mut out);
  assert_eq!(early, Err(Refused::OutOfTurn));
  assert_eq!(connection.waiting(), Waiting::Command);
  send(&fixture, &alice, &mut connection, COMMIT_NEWDIR)
    .1
    .unwrap();
  connection.commit(start(), &sources, &mut out).unwrap();
  out.clear();
  let open_root = ADD_NEWDIR[0].as_bytes();
  let turn = connection.receive(Some(&sources), open_root, &mut out, PLENTY);
  assert!(matches!(turn, Ok(Turn::Hook { .. })), "{turn:?}");
  let next = ADD_NEWDIR[1].as_bytes();
  let refused = connection.receive(Some(&sources), next, &mut out, PLENTY);
  assert_eq!(refused, Err(Refused::OutOfTurn));
  assert_eq!(
    connection.commit(start(), &sources, &mut out),
    Err(Refused::OutOfTurn)
  );
  let made = Made {
    revision: 7,
    date: DATE,
    author: None,
    post_commit_error: None,
  };
  assert_eq!(
    connection.committed(&made, &mut out),
    Err(Refused::OutOfTurn)
  );
  let unmade = connection.not_committed(&Uncommitted::NotServed, &mut out);
  assert_eq!(unmade, Err(Refused::OutOfTurn));
  assert!(out.is_empty());
  assert_eq!(connection.waiting(), Waiting::Writing);
  let answered = connection.answer_hook(Ok(()), &sources, &mut out);
  assert_eq!(answered, Ok(Turn::Took { consumed: 0 }));
  assert_eq!(connection.waiting(), Waiting::Edit);
}

/// A-656 (the caller's errors around a commit): begin a commit where none was asked; answer a work
/// where none was accepted, both ways; ask for a commit, then call `receive` where its start is
/// awaited; expect each `OutOfTurn` with nothing written and the connection where it stood, the
/// commit then begun.
#[test]
fn a_commit_call_out_of_turn_is_the_callers_error_and_changes_nothing() {
  let fixture = fixture();
  let alice = FixtureGuards::default();
  let sources = fixture.sources(&alice);
  let mut connection = established_on(&alices_tunnel(), EXTERNALLY);
  let mut out = Vec::new();
  let refused = connection.commit(start(), &sources, &mut out);
  assert_eq!(refused, Err(Refused::OutOfTurn));
  let made = Made {
    revision: 7,
    date: DATE,
    author: None,
    post_commit_error: None,
  };
  assert_eq!(
    connection.committed(&made, &mut out),
    Err(Refused::OutOfTurn)
  );
  let unmade = connection.not_committed(&Uncommitted::NotServed, &mut out);
  assert_eq!(unmade, Err(Refused::OutOfTurn));
  assert_eq!(connection.waiting(), Waiting::Command);
  let asked = connection.receive(Some(&sources), COMMIT_NEWDIR, &mut out, PLENTY);
  assert!(matches!(asked, Ok(Turn::CommitAsked { .. })), "{asked:?}");
  let early = connection.receive(Some(&sources), CLOSE_EDIT, &mut out, PLENTY);
  assert_eq!(early, Err(Refused::OutOfTurn));
  assert!(out.is_empty());
  assert_eq!(connection.waiting(), Waiting::Writing);
  connection.commit(start(), &sources, &mut out).unwrap();
  let begun = "( success ( ( ) 0: ) ) ( success ( ) ) ";
  assert_eq!(String::from_utf8_lossy(&out), begun);
}
