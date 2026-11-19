//! The connection's tests (A-656; §4.17.4 "5. The wire", piece 7).
//!
//! The oracle is stock `svnserve` 1.15.0's whole sessions recorded for the reporter
//! (`crates/svn/fixtures/report/`, each file's SHA-256 in `SHA256SUMS`): each session's client bytes
//! are handed over an item at a time, each when the connection asks for more, as a stock client
//! sends them after reading each answer; what the connection writes over the fixtures' repository
//! must be what `svnserve` wrote: its greeting less `svndiff1`, every item byte for byte (property
//! lists as sets: `svnserve` writes them in APR's seeded hash order), its drive call for call; up to
//! the first command slates does not serve yet, which must be answered as one `svnserve` does not
//! know. Beside them: `reparent` inside and outside the repository, commands not served yet, items
//! that are no command, calls without the repository's sources, the smallest budget, what the
//! connection waits for, a tunnel's user, and a repository that admits no one.

use super::*;
use crate::report::drive::Sources;
use crate::report::drive::green::{FixtureGuards, Green, guards_of, normalize, repository};
use crate::report::drive::green::texts_by_md5;
use crate::report::recorded::{
  ROOT_URL, Recorded, SESSIONS, UUID, command, drive_items, finished_report, items, read_drive,
  value_of,
};
use crate::index::Index;
use std::collections::BTreeMap;

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

/// The fixtures' repository.
struct Fixture {
  /// The index.
  index: Index,
  /// The green.
  green: Green,
}

/// The fixtures' repository built.
fn fixture() -> Fixture {
  let (index, green) = repository();
  Fixture { index, green }
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
  let sources = Sources {
    index: &fixture.index,
    repository: &fixture.green,
    guards: &guards,
  };
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
        assert_eq!(connection.open(&fixtures_repository(), &mut out), Ok(Access::Asked));
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
      let named = |tree: &Tree| {
        matches!(tree, Tree::List(record) if record.len() >= 2 && matches!(record[0], Tree::Bytes(_)))
      };
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
    let at = greeting.windows(SVNDIFF1.len()).position(|window| window == SVNDIFF1).unwrap();
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
  assert_eq!(normalize(our_drive, texts), normalize(their_drive, texts), "{name}: the drive");
  let after = ours.len() - our_end;
  assert_items(name, &ours[our_end..], &theirs[their_end..their_end + after], hidden);
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
        assert_eq!(recorded.name, "hostile-report-malformed-set-path.1", "{refused:?}");
        ours.len().min(items(recorded.server).len())
      }
      Ending::Waiting => ours.len(),
    };
    assert_as_svnserve(recorded.name, &ours[..compared], recorded.server, &texts, &mut hidden);
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
    let recorded = SESSIONS.iter().copied().find(|session| session.name == name).unwrap();
    assert_eq!(replay(&fixture, recorded, SMALLEST), replay(&fixture, recorded, PLENTY), "{name}");
  }
}

/// A stock 1.15 client's answer to the greeting for `url`, and its anonymous authentication.
fn opening(url: &str) -> Vec<u8> {
  format!(
    "( 2 ( edit-pipeline svndiff1 accepts-svndiff2 absent-entries depth mergeinfo log-revprops ) \
     {}:{url} 10:SVN/1.15.0 ( ) ) ( ANONYMOUS ( 0: ) ) ",
    url.len()
  )
  .into_bytes()
}

/// A connection established on the fixtures' repository at `/trunk`, anonymously: its output so far
/// and the connection.
fn established(fixture: &Fixture, guards: &FixtureGuards) -> (Connection, Vec<u8>) {
  let sources = Sources {
    index: &fixture.index,
    repository: &fixture.green,
    guards,
  };
  let mut out = Vec::new();
  let mut connection = Connection::new(LIMITS, &mut out).unwrap();
  let opening = opening("svn://127.0.0.1/repo/trunk");
  let mut rest = opening.as_slice();
  while connection.session().is_none() {
    let turn = connection.receive(None::<&Sources<'_, Green, FixtureGuards>>, rest, &mut out, PLENTY);
    match turn.unwrap() {
      Turn::Opened { consumed, .. } => {
        rest = &rest[consumed..];
        connection.open(&fixtures_repository(), &mut out).unwrap();
      }
      Turn::Took { consumed } | Turn::Established { consumed, .. } => rest = &rest[consumed..],
      other => panic!("{other:?}"),
    }
  }
  let _ = sources;
  out.clear();
  (connection, out)
}

/// Sends `command` on an established connection and returns what it wrote and how it ended.
fn send(
  fixture: &Fixture,
  guards: &FixtureGuards,
  connection: &mut Connection,
  command: &[u8],
) -> (String, Result<String, Refused>) {
  let sources = Sources {
    index: &fixture.index,
    repository: &fixture.green,
    guards,
  };
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
  let (mut connection, _) = established(&fixture, &guards);
  let (out, turn) = send(&fixture, &guards, &mut connection, b"( reparent ( 25:svn://127.0.0.1/repo/tags ) ) ");
  assert_eq!(out, "( success ( ( ) 0: ) ) ( success ( ) ) ");
  assert!(turn.unwrap().starts_with("Took"));
  let (out, _) = send(&fixture, &guards, &mut connection, b"( check-path ( 5:a.txt ( 6 ) ) ) ");
  assert!(out.ends_with("( success ( none ) ) "), "{out}");
  let (out, _) = send(&fixture, &guards, &mut connection, b"( reparent ( 23:svn://example.org/other ) ) ");
  let message = "'svn://example.org/other' is not the same repository as 'svn://127.0.0.1/repo'";
  let failure = format!("( failure ( ( 170000 {}:{message} 0: 0 ) ) ) ", message.len());
  assert_eq!(out, format!("( success ( ( ) 0: ) ) {failure}"));
  let (out, turn) = send(&fixture, &guards, &mut connection, b"( reparent ( 5 ) ) ");
  assert_eq!((out.as_str(), turn), ("", Err(Refused::Malformed)));
}

/// A-656 (commands not served yet, and items that are no command): send `commit`, then
/// `get-latest-rev`; then an item that is a number; expect "Unknown editor command 'commit'"
/// (210001) and the connection going on, the youngest revision answered, then the connection's end.
#[test]
fn a_command_not_served_yet_is_answered_and_the_session_goes_on() {
  let fixture = fixture();
  let guards = FixtureGuards::anonymous();
  let (mut connection, _) = established(&fixture, &guards);
  let (out, turn) = send(&fixture, &guards, &mut connection, b"( commit ( 3:msg ) ) ");
  let message = "Unknown editor command 'commit'";
  assert_eq!(out, format!("( failure ( ( 210001 {}:{message} 0: 0 ) ) ) ", message.len()));
  assert_eq!(turn, Ok(String::from("Unserved { command: \"commit\", consumed: 21 }")));
  let (out, _) = send(&fixture, &guards, &mut connection, b"( get-latest-rev ( ) ) ");
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
  let (mut connection, _) = established(&fixture, &guards);
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
  let recorded = SESSIONS.iter().copied().find(|session| session.name == "checkout-r1.1").unwrap();
  let guards = FixtureGuards::anonymous();
  let sources = Sources {
    index: &fixture.index,
    repository: &fixture.green,
    guards: &guards,
  };
  let client = items(recorded.client);
  let mut out = Vec::new();
  let mut connection = Connection::new(LIMITS, &mut out).unwrap();
  let mut seen = vec![connection.waiting()];
  let (mut next, mut held, mut established) = (client.iter(), Vec::new(), false);
  loop {
    let given = established.then_some(&sources);
    match connection.receive(given, &held, &mut out, SMALLEST).unwrap() {
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

/// A-656 (`serve.c` `update`'s `must_have_access` on the session's path joined to the target): ask
/// to update `secret` beneath `/trunk`, which the anonymous principal may not read, then the readable
/// `dir`; expect "Authorization failed" (170001) with no auth request before it, then the report
/// awaited after a trivial auth request.
#[test]
fn an_update_of_a_target_the_principal_may_not_read_is_refused() {
  let fixture = fixture();
  let guards = FixtureGuards::anonymous();
  let (mut connection, _) = established(&fixture, &guards);
  let (out, turn) = send(
    &fixture,
    &guards,
    &mut connection,
    b"( update ( ( 6 ) 6:secret true infinity false false ) ) ",
  );
  let message = "Authorization failed";
  assert_eq!(out, format!("( failure ( ( 170001 {}:{message} 0: 0 ) ) ) ", message.len()));
  assert!(turn.unwrap().starts_with("Took"));
  assert_eq!(connection.waiting(), Waiting::Command);
  let (out, _) = send(
    &fixture,
    &guards,
    &mut connection,
    b"( update ( ( 6 ) 3:dir true infinity false false ) ) ",
  );
  assert_eq!(out, "( success ( ( ) 0: ) ) ");
  assert_eq!(connection.waiting(), Waiting::Report);
}
