//! The read commands' tests (A-613, §4.17.4 "4. Reads" and piece 7's second part). The golden vectors are transcripts
//! of stock `svnserve` 1.15.0 answering stock `svn` 1.15.0 and scripted clients, recorded by
//! `crates/svn/fixtures/reads/make-fixtures.py` (each file's SHA-256 in `SHA256SUMS`): the repository is rebuilt from
//! the recorder's own record of it (`repository.txt`), every client command of a session is answered, and the answers
//! must be `svnserve`'s, item for item, a property list's order aside (APR seeds `svnserve`'s hash order per table, so
//! three recordings gave three orders; module documentation). Then the parameters, the dates and the refusals the
//! transcripts do not reach, case by case, and hostile commands, which never panic.

use super::*;
use crate::index::{Action, ChangedPath, CopySource, RevisionRecord, Text};
use crate::ra_svn::{Bounds, Parsed, Parser};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use std::collections::BTreeMap;

/// A transaction name of the form §4.17.4 item 3.1 gives.
const TRANSACTION: &str = "0-1z";

/// The recorded repository's UUID (`make-fixtures.py` `UUID`).
const UUID: &str = "00000000-0000-4000-8000-00000000a613";

/// The recorded repository's URL (`make-fixtures.py` `URL`): a session's base is the path past it.
const URL: &str = "svn://127.0.0.1:36913/repo";

/// The recorder's record of the repository the transcripts were made against.
const REPOSITORY: &str = include_str!("../../fixtures/reads/repository.txt");

/// One node of the green: its kind and its property list.
type GreenNode = (NodeKind, PropertyList);

/// The green the transcripts' repository holds: every revision's tree, the property lists, the texts and the revision
/// properties.
#[derive(Debug, Default)]
struct Green {
  /// Every revision's whole tree.
  trees: Vec<BTreeMap<String, GreenNode>>,
  /// The property lists, by identity.
  lists: BTreeMap<ContentIdentity, Vec<(Vec<u8>, Vec<u8>)>>,
  /// The texts, by identity.
  texts: BTreeMap<ContentIdentity, Vec<u8>>,
  /// Every revision's properties.
  revisions: Vec<Vec<(Vec<u8>, Vec<u8>)>>,
}

/// The most bytes of a text the test green hands over at once: less than one of `get-file`'s strings, so each string
/// is gathered from several pieces, as from a chunked text.
const PIECE: usize = 1_000;

impl Contents for Green {
  fn entries(&self, directory: &str, revision: Revision) -> Option<Vec<String>> {
    let tree = self.trees.get(usize::try_from(revision).ok()?)?;
    if tree.get(directory)?.0 != NodeKind::Directory {
      return None;
    }
    Some(
      tree
        .keys()
        .filter(|place| place.as_str() != directory && parent(place) == Some(directory))
        .filter_map(|place| Some(place.rsplit('/').next()?.to_owned()))
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

impl Repository for Green {
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

/// The index and the green of the transcripts' repository.
struct Fixture {
  /// The index.
  index: Index,
  /// The green.
  green: Green,
}

/// Bytes from hex.
fn unhex(text: &str) -> Vec<u8> {
  (0..text.len())
    .step_by(2)
    .map(|at| u8::from_str_radix(text.get(at..at + 2).unwrap(), 16).unwrap())
    .collect()
}

/// A content identity counted from `number`.
fn identity(number: usize) -> ContentIdentity {
  let mut identity = [0_u8; 32];
  identity[..8].copy_from_slice(&u64::try_from(number).unwrap().to_le_bytes());
  identity
}

/// Whether `place` is `path` or beneath it.
fn at_or_beneath(place: &str, path: &str) -> bool {
  place == path
    || path == "/"
    || place
      .strip_prefix(path)
      .is_some_and(|rest| rest.starts_with('/'))
}

impl Fixture {
  /// The repository `repository.txt` records, rebuilt revision by revision.
  fn new() -> Self {
    let mut fixture = Self {
      index: Index::new(),
      green: Green::default(),
    };
    let root = (NodeKind::Directory, PropertyList::Empty);
    fixture
      .green
      .trees
      .push(BTreeMap::from([("/".to_owned(), root)]));
    fixture.green.revisions.push(Vec::new());
    let mut rows: BTreeMap<u64, Vec<&str>> = BTreeMap::new();
    for line in REPOSITORY.lines() {
      let words: Vec<&str> = line.split(' ').collect();
      match words[0] {
        "revision" if words[1] == "0" => fixture.green.revisions[0]
          .push((words[2].as_bytes().to_vec(), words[3].as_bytes().to_vec())),
        "revision" => {
          let revision = usize::try_from(words[1].parse::<u64>().unwrap()).unwrap();
          while fixture.green.revisions.len() <= revision {
            fixture.green.revisions.push(Vec::new());
          }
          fixture.green.revisions[revision].push((words[2].as_bytes().to_vec(), unhex(words[3])));
        }
        "row" => rows
          .entry(words[1].parse().unwrap())
          .or_default()
          .push(line),
        other => panic!("an unknown record {other}"),
      }
    }
    for lines in rows.values() {
      fixture.commit(lines);
    }
    fixture
  }

  /// Commits one revision's rows to the index and to the green's trees.
  fn commit(&mut self, lines: &[&str]) {
    let mut tree = self.green.trees.last().unwrap().clone();
    let changes: Vec<ChangedPath> = lines.iter().map(|line| self.row(line, &mut tree)).collect();
    self
      .index
      .append(
        RevisionRecord {
          transaction: TRANSACTION.to_owned(),
          changes,
        },
        usize::MAX,
      )
      .unwrap();
    self.green.trees.push(tree);
  }

  /// One row: its changed-path record, applied to `tree`.
  fn row(&mut self, line: &str, tree: &mut BTreeMap<String, GreenNode>) -> ChangedPath {
    let words: Vec<&str> = line.split(' ').collect();
    let (path, kind) = (
      words[2].to_owned(),
      if words[3] == "dir" {
        NodeKind::Directory
      } else {
        NodeKind::File
      },
    );
    let action = match words[4] {
      "add" => Action::Add,
      "change" => Action::Modify,
      "delete" => Action::Delete,
      "replace" => Action::Replace,
      other => panic!("an unknown action {other}"),
    };
    let field = |name: &str| {
      words
        .iter()
        .position(|word| *word == name)
        .map(|at| words[at + 1])
    };
    let copy_from = field("copy").map(|source| CopySource {
      path: source.to_owned(),
      revision: words[words.iter().position(|word| *word == "copy").unwrap() + 2]
        .parse()
        .unwrap(),
    });
    let had = tree.get(&path).is_some_and(|node| node.1.has_mergeinfo());
    let properties = field("props").map(|list| self.list(list));
    let text = field("text").map(|text| {
      let bytes = unhex(text);
      let (md5, sha1) = (unhex(field("md5").unwrap()), unhex(field("sha1").unwrap()));
      let mut identity = [0_u8; 32];
      identity[..16].copy_from_slice(&md5);
      let size = u64::try_from(bytes.len()).unwrap();
      self.green.texts.insert(identity, bytes);
      Text {
        identity,
        md5: md5.try_into().unwrap(),
        sha1: sha1.try_into().unwrap(),
        size,
      }
    });
    if matches!(action, Action::Delete | Action::Replace) {
      tree.retain(|place, _| !at_or_beneath(place, &path));
    }
    match action {
      Action::Delete => {}
      _ if copy_from.is_some() => {
        let source = copy_from.as_ref().unwrap();
        let from = self.green.trees[usize::try_from(source.revision).unwrap()].clone();
        for (place, node) in from
          .iter()
          .filter(|(place, _)| at_or_beneath(place, &source.path))
        {
          let moved = format!("{path}{}", place.get(source.path.len()..).unwrap());
          tree.insert(moved, *node);
        }
      }
      _ => {
        let held = tree.get(&path).map_or(PropertyList::Empty, |node| node.1);
        tree.insert(path.clone(), (kind, properties.unwrap_or(held)));
      }
    }
    let has = properties.is_some_and(PropertyList::has_mergeinfo);
    ChangedPath {
      path,
      action,
      kind,
      copy_from,
      text,
      properties,
      mergeinfo_modified: properties.is_some() && (had || has),
    }
  }

  /// A property list from its record, held under a fresh identity; an empty list is none.
  fn list(&mut self, record: &str) -> PropertyList {
    let properties: Vec<(Vec<u8>, Vec<u8>)> = record
      .split(',')
      .filter(|pair| !pair.is_empty())
      .map(|pair| {
        let (name, value) = pair.split_once('=').unwrap();
        (unhex(name), unhex(value))
      })
      .collect();
    if properties.is_empty() {
      return PropertyList::Empty;
    }
    let has_mergeinfo = properties.iter().any(|(name, _)| name == b"svn:mergeinfo");
    let identity = identity(self.green.lists.len() + 1);
    self.green.lists.insert(identity, properties);
    PropertyList::Held {
      identity,
      has_mergeinfo,
    }
  }
}

/// Every item of a transcript, as the item codec reads them.
fn items(bytes: &[u8]) -> Vec<Value<'_>> {
  let bounds = Bounds {
    item_bytes: bytes.len(),
    word_bytes: 64,
    depth: 16,
    nodes: bytes.len(),
  };
  let mut parser = Parser::new(bounds);
  let mut found = Vec::new();
  let mut at = 0;
  while bytes[at..].iter().any(|byte| !byte.is_ascii_whitespace()) {
    match parser.parse(&bytes[at..]).unwrap() {
      Parsed::Item { value, consumed } => {
        found.push(value);
        at += consumed;
      }
      Parsed::NeedMore { .. } => panic!("a transcript ends inside an item"),
    }
  }
  found
}

/// An answer written out with its property lists in name order, so `svnserve`'s seeded hash order compares as a set.
fn normalized(value: &Value<'_>) -> String {
  match value {
    Value::List(items) => {
      let pairs = !items.is_empty() && items.iter().all(|item| matches!(item, Value::List(_)));
      let mut written: Vec<String> = items.iter().map(normalized).collect();
      if pairs {
        written.sort();
      }
      format!("( {}) ", written.concat())
    }
    other => {
      let mut out = Vec::new();
      write(other, &mut out).unwrap();
      String::from_utf8_lossy(&out).into_owned()
    }
  }
}

/// The commands whose answers hold property lists, compared as sets.
const PROPERTY_LISTS: [&str; 5] = [
  "rev-proplist",
  "get-iprops",
  "get-dir",
  "get-locations",
  "get-file",
];

/// The room each slice of a streamed answer is given in the replays: one byte, so each slice is the least a slice
/// may be, one item.
const SLICE_ROOM: usize = 1;

/// A command's outcome with a streamed answer written whole, a slice of [`SLICE_ROOM`] at a time.
fn drained(
  outcome: Outcome,
  fixture: &Fixture,
  session: &Session<'_>,
  out: &mut Vec<u8>,
) -> Outcome {
  let Outcome::Streaming(mut stream) = outcome else {
    return outcome;
  };
  while stream
    .resume(&fixture.index, &fixture.green, session, out, SLICE_ROOM)
    .unwrap()
    == Progress::More
  {}
  Outcome::Answered
}

/// Replays a recorded session through the handler: each client command after the handshake answered, each answer
/// `svnserve`'s, the commands in `elsewhere` (another piece's) skipped with their two items, a malformed command ending
/// the session as `svnserve` ended it; answers how many commands were answered here.
fn replay(name: &str, client: &[u8], server: &[u8], elsewhere: &[&str]) -> usize {
  let fixture = Fixture::new();
  let answers = items(server);
  let commands = items(client);
  let base = session_base(&commands[0]);
  let session = Session {
    base: &base,
    uuid: UUID,
  };
  let mut at = 4;
  let mut answered = 0;
  for command in commands.iter().skip(2) {
    let word = command_word(name, command);
    let mut out = Vec::new();
    let outcome = answer(&fixture.index, &fixture.green, &session, command, &mut out).unwrap();
    match drained(outcome, &fixture, &session, &mut out) {
      Outcome::Streaming(_) => panic!("{name}: a drained stream streams"),
      Outcome::Answered => {
        at = same_answer(name, word, &items(&out), &answers, at);
        answered += 1;
      }
      Outcome::Elsewhere => {
        assert!(
          elsewhere.contains(&word),
          "{name}: {word} is another piece's"
        );
        at += 2;
      }
      Outcome::Close => return closed(name, word, at, answers.len(), answered),
    }
  }
  assert_eq!(
    at,
    answers.len(),
    "{name}: svnserve answered more than was replayed"
  );
  answered
}

/// The session's base: the path of the client's URL past the repository's (its greeting's third item).
fn session_base(greeting: &Value<'_>) -> String {
  let Value::List(parts) = greeting else {
    panic!("a greeting that is no list");
  };
  let Some(Value::String(url)) = parts.get(2) else {
    panic!("a greeting without its URL");
  };
  let path = std::str::from_utf8(url).unwrap().strip_prefix(URL).unwrap();
  if path.is_empty() {
    "/".to_owned()
  } else {
    path.to_owned()
  }
}

/// A client command's word.
fn command_word<'a>(name: &str, command: &'a Value<'_>) -> &'a str {
  match command {
    Value::List(parts) => match parts.first() {
      Some(Value::Word(word)) => word,
      _ => panic!("{name}: a command without its word"),
    },
    _ => panic!("{name}: a client item that is no command"),
  }
}

/// Holds one answer to `svnserve`'s items from `at`, property lists as sets for the commands that hold them; answers
/// where the next answer starts.
fn same_answer(
  name: &str,
  word: &str,
  mine: &[Value<'_>],
  answers: &[Value<'_>],
  at: usize,
) -> usize {
  let theirs = &answers[at..at + mine.len()];
  let (mine, theirs): (Vec<String>, Vec<String>) = if PROPERTY_LISTS.contains(&word) {
    (
      sorted(mine.iter().map(normalized).collect()),
      sorted(theirs.iter().map(normalized).collect()),
    )
  } else {
    (
      mine.iter().map(|value| format!("{value:?}")).collect(),
      theirs.iter().map(|value| format!("{value:?}")).collect(),
    )
  };
  assert_eq!(mine, theirs, "{name}: {word}");
  at + mine.len()
}

/// Items in byte order of their written form: an answer `svnserve` writes in hash order, compared as a set.
fn sorted(mut written: Vec<String>) -> Vec<String> {
  written.sort();
  written
}

/// A malformed command: `svnserve` wrote nothing after it; answers the commands answered before it.
fn closed(name: &str, word: &str, at: usize, answers: usize, answered: usize) -> usize {
  assert_eq!(
    at, answers,
    "{name}: svnserve wrote after the malformed {word}"
  );
  answered
}

/// A recorded session's two transcripts.
macro_rules! transcript {
  ($name:literal) => {
    (
      $name,
      include_bytes!(concat!("../../fixtures/reads/", $name, ".1.client.bin")).as_slice(),
      include_bytes!(concat!("../../fixtures/reads/", $name, ".1.server.bin")).as_slice(),
    )
  };
}

/// A-613: the scripted sessions: every command answered as `svnserve` answered it (`get-latest-rev`, `get-dated-rev`
/// at, between, before and after the revisions' dates and refusing two bogus dates; `rev-proplist` and `rev-prop`,
/// one absent and one past the youngest; `check-path` and `stat` of the root, files, directories, absent paths, a
/// deleted file before and after, a path to canonicalize, a revision past the youngest; `get-mergeinfo` inherited,
/// explicit, nearest-ancestor, with descendants, through a copy, with no path, with a word Subversion does not know and
/// of an absent path; `get-iprops` through a copy, of the root, of an absent path and without a revision), an unknown
/// command refused with the session going on, and a malformed one ending it.
#[test]
fn the_scripted_sessions_are_answered_as_svnserve_answered_them() {
  let sessions = [
    (transcript!("latest-and-dated"), 7),
    (transcript!("revision-properties"), 7),
    (transcript!("check-path"), 8),
    (transcript!("stat"), 7),
    (transcript!("get-mergeinfo"), 9),
    (transcript!("get-iprops"), 6),
    (transcript!("get-dir"), 7),
    (transcript!("get-locations"), 8),
    (transcript!("hostile-locations"), 0),
    (transcript!("get-file"), 18),
    (transcript!("hostile-get-file"), 0),
    (transcript!("list"), 16),
    (transcript!("hostile-list"), 0),
    (transcript!("get-location-segments"), 16),
    (transcript!("unknown-command"), 2),
    (transcript!("hostile-malformed"), 0),
  ];
  for ((name, client, server), commands) in sessions {
    assert_eq!(replay(name, client, server, &[]), commands, "{name}");
  }
}

/// A-613: stock `svn` 1.15.0's own sessions, `info` of the root and at a date, and a revision property read and
/// listed, answered as `svnserve` answered them; the lock it asks for is the lock piece's.
#[test]
fn stock_svn_sessions_are_answered_as_svnserve_answered_them() {
  let sessions = [
    (transcript!("svn-info"), 3),
    (transcript!("svn-info-dated"), 4),
    (transcript!("svn-revprop-get"), 1),
    (transcript!("svn-revprop-list"), 1),
    (transcript!("svn-info-file"), 5),
    (transcript!("svn-proplist-inherited"), 4),
    (transcript!("svn-cat"), 4),
    (transcript!("svn-cat-big"), 3),
    (transcript!("svn-ls"), 2),
  ];
  for ((name, client, server), commands) in sessions {
    assert_eq!(
      replay(name, client, server, &["get-lock", "get-locks"]),
      commands,
      "{name}"
    );
  }
}

/// A-613: the recorder's `SHA256SUMS` names every transcript the tests read, and the recorder's record of the
/// repository; the Studio run checks each sum with `shasum -a 256 -c`.
#[test]
fn every_fixture_has_its_recorded_sum() {
  let sums = include_str!("../../fixtures/reads/SHA256SUMS");
  for name in [
    "get-location-segments",
    "list",
    "hostile-list",
    "svn-ls",
    "get-file",
    "hostile-get-file",
    "svn-cat",
    "svn-cat-big",
    "get-dir",
    "get-locations",
    "hostile-locations",
    "svn-info-file",
    "svn-proplist-inherited",
    "latest-and-dated",
    "revision-properties",
    "check-path",
    "stat",
    "get-mergeinfo",
    "get-iprops",
    "unknown-command",
    "hostile-malformed",
    "svn-info",
    "svn-info-dated",
    "svn-revprop-get",
    "svn-revprop-list",
  ] {
    for side in ["client", "server"] {
      let file = format!("  {name}.1.{side}.bin");
      assert!(
        sums
          .lines()
          .any(|line| line.ends_with(&file) && line.len() == 64 + file.len()),
        "{file}"
      );
    }
  }
  assert!(sums.lines().any(|line| line.ends_with("  repository.txt")));
}

/// One command's answer against the fixture's repository.
fn answered(command: &[u8]) -> (Outcome, Vec<u8>) {
  let fixture = Fixture::new();
  let value = items(command).remove(0);
  let mut out = Vec::new();
  let outcome = answer(
    &fixture.index,
    &fixture.green,
    &Session {
      base: "/",
      uuid: UUID,
    },
    &value,
    &mut out,
  )
  .unwrap();
  (outcome, out)
}

/// A-613: parameters as `svn_ra_svn__parse_tuple` reads them: missing or of the wrong kind ends the connection, nothing
/// written; parameters past the prototype are ignored; a C string ends at its first NUL; a number past 2^63 - 1 is a
/// negative revision, refused as invalid after the authentication request.
#[test]
fn parameters_are_read_as_svn_ra_svn_parse_tuple_reads_them() {
  for malformed in [
    b"( check-path ( ) ) ".as_slice(),
    b"( check-path ( 5:trunk ) ) ",
    b"( check-path ( 5:trunk 3 ) ) ",
    b"( check-path ( 5:trunk ( word ) ) ) ",
    b"( stat ( 7 ( ) ) ) ",
    b"( rev-prop ( 1 ) ) ",
    b"( get-dated-rev ( word ) ) ",
    b"( get-mergeinfo ( ( 5 ) ( ) inherited false ) ) ",
    b"( get-mergeinfo ( ( 5:trunk ) ( ) inherited maybe ) ) ",
    b"( get-iprops ( 5:trunk ) ) ",
    b"( get-latest-rev ) ",
    b"word ",
    b"( 5:check ( ) ) ",
  ] {
    assert_eq!(
      answered(malformed),
      (Outcome::Close, Vec::new()),
      "{}",
      String::from_utf8_lossy(malformed)
    );
  }
  let (_, extra) = answered(b"( check-path ( 5:trunk ( 1 ) extra 7 ( ) ) ) ");
  assert_eq!(extra, b"( success ( ( ) 0: ) ) ( success ( dir ) ) ");
  let (_, nul) = answered(b"( check-path ( 11:trunk\0a.txt ( ) ) ) ");
  assert_eq!(nul, b"( success ( ( ) 0: ) ) ( success ( dir ) ) ");
  let (_, huge) = answered(b"( rev-proplist ( 18446744073709551615 ) ) ");
  assert_eq!(
    huge,
    b"( success ( ( ) 0: ) ) ( failure ( ( 160006 28:Invalid revision number '-1' 0: 0 ) ) ) "
  );
}

/// A-613: commands another piece answers are handed back untouched, and the main set's every other command name is
/// unknown, refused with no authentication request first.
#[test]
fn other_pieces_commands_are_handed_back_and_unknown_ones_refused() {
  for name in ELSEWHERE {
    let command = format!("( {name} ( ) ) ");
    assert_eq!(
      answered(command.as_bytes()),
      (Outcome::Elsewhere, Vec::new()),
      "{name}"
    );
  }
  let (outcome, out) = answered(b"( get-latest ( ) ) ");
  assert_eq!(outcome, Outcome::Answered);
  assert_eq!(
    out,
    b"( failure ( ( 210001 35:Unknown editor command 'get-latest' 0: 0 ) ) ) "
  );
}

/// A-613: dates as `svn_time_from_cstring` and APR read them and `svn_time_to_cstring` writes them: the format's
/// fields in any width, digits alone (no sign, no space), a month outside 1-12 or a time before 1970 refused, every
/// other field taken as it is (the 32nd of January is the 1st of February), microseconds kept; and the dated revision
/// search's edges.
#[test]
fn dates_are_read_and_written_as_subversion_reads_and_writes_them() {
  let read: [(&str, Option<&str>); 15] = [
    (
      "1970-01-01T00:00:00.000000Z",
      Some("1970-01-01T00:00:00.000000Z"),
    ),
    (
      "1970-01-01T00:00:01.000002Z",
      Some("1970-01-01T00:00:01.000002Z"),
    ),
    (
      "2026-01-03T03:00:00.000000Z",
      Some("2026-01-03T03:00:00.000000Z"),
    ),
    ("2026-1-3T3:0:0.5Z", Some("2026-01-03T03:00:00.000005Z")),
    (
      "2026-01-32T00:00:00.000000Z",
      Some("2026-02-01T00:00:00.000000Z"),
    ),
    (
      "2024-02-29T23:59:59.999999Z",
      Some("2024-02-29T23:59:59.999999Z"),
    ),
    (
      "2100-03-01T00:00:00.000000Z",
      Some("2100-03-01T00:00:00.000000Z"),
    ),
    ("2026-00-01T00:00:00.000000Z", None),
    ("2026-13-01T00:00:00.000000Z", None),
    ("1969-12-31T23:59:59.000000Z", None),
    ("2026-01-04", None),
    ("2026-01-04T00:00:00Z", None),
    (" 2026-01-04T00:00:00.000000Z", None),
    ("+2026-01-04T00:00:00.000000Z", None),
    ("", None),
  ];
  for (text, written) in read {
    assert_eq!(
      parse_time(text.as_bytes()).map(format_time).as_deref(),
      written,
      "{text}"
    );
  }
  assert_eq!(parse_time(b"1970-01-01T00:00:01.000002Z"), Some(1_000_002));
  let dated = [
    ("2026-01-01T00:00:00.000000Z", 0),
    ("2026-01-01T01:00:00.000000Z", 1),
    ("2026-01-07T07:00:00.000000Z", 7),
    ("2026-01-06T06:00:00.000001Z", 6),
  ];
  for (date, revision) in dated {
    let command = format!("( get-dated-rev ( {}:{date} ) ) ", date.len());
    let (_, out) = answered(command.as_bytes());
    let expected = format!("( success ( ( ) 0: ) ) ( success ( {revision} ) ) ");
    assert_eq!(String::from_utf8(out).unwrap(), expected, "{date}");
  }
}

/// The session at the repository's root.
const ROOT_SESSION: Session<'static> = Session {
  base: "/",
  uuid: UUID,
};

/// One command's whole answer against `repository`, a streamed rest resumed in `room` until it is whole and once more;
/// answers the outcome, the answer and how many slices the stream took.
fn streamed<R: Repository>(
  fixture: &Fixture,
  repository: &R,
  command: &[u8],
  room: usize,
) -> (Outcome, Vec<u8>, usize) {
  let value = items(command).remove(0);
  let mut out = Vec::new();
  let outcome = answer(&fixture.index, repository, &ROOT_SESSION, &value, &mut out).unwrap();
  let Outcome::Streaming(mut stream) = outcome.clone() else {
    return (outcome, out, 0);
  };
  let mut slices = 1;
  while stream
    .resume(&fixture.index, repository, &ROOT_SESSION, &mut out, room)
    .unwrap()
    == Progress::More
  {
    slices += 1;
  }
  let whole = out.len();
  let again = stream.resume(&fixture.index, repository, &ROOT_SESSION, &mut out, room);
  assert_eq!((again, out.len()), (Ok(Progress::Done), whole));
  (outcome, out, slices)
}

/// The lengths of an answer's strings, in order.
fn string_lengths(answer: &[u8]) -> Vec<usize> {
  items(answer)
    .iter()
    .filter_map(|item| match item {
      Value::String(bytes) => Some(bytes.len()),
      _ => None,
    })
    .collect()
}

/// A-613 part 2 (`get-file`, item 11): a file's text streams a slice at a time in the room the caller gives, one
/// string a slice in a room of one byte and the end in one more, the whole answer in one slice in a room past its
/// size, the same bytes either way; a finished stream writes nothing more.
#[test]
fn a_files_text_streams_a_slice_at_a_time_in_the_room_given() {
  let fixture = Fixture::new();
  let command = b"( get-file ( 13:trunk/big.txt ( 9 ) false true ) ) ";
  let (_, narrow, narrow_slices) = streamed(&fixture, &fixture.green, command, 1);
  let (_, wide, wide_slices) = streamed(&fixture, &fixture.green, command, usize::MAX);
  assert_eq!(narrow, wide);
  assert_eq!((narrow_slices, wide_slices), (4, 1));
  assert_eq!(string_lengths(&wide), [4_096, 4_096, 1_818, 0]);
}

/// A-613 part 2 (`get-file`): a file added without a text has the MD5 of no bytes, the checksum
/// `svn_fs_file_checksum` computes when forced to, and its text is the empty string alone.
#[test]
fn a_file_without_a_text_has_the_md5_of_no_bytes() {
  let mut fixture = Fixture::new();
  fixture.commit(&["row 12 /trunk/blank.txt file add"]);
  let command = b"( get-file ( 15:trunk/blank.txt ( ) false true ) ) ";
  let (_, out, _) = streamed(&fixture, &fixture.green, command, 1);
  assert_eq!(
    String::from_utf8(out).unwrap(),
    concat!(
      "( success ( ( ) 0: ) ) ",
      "( success ( ( 32:d41d8cd98f00b204e9800998ecf8427e ) 12 ( ) ) ) ",
      "0: ( success ( ) ) "
    )
  );
}

/// The fixture's green with every text lost past its first `kept` bytes.
struct Torn<'green> {
  /// The green.
  green: &'green Green,
  /// The bytes of each text still held.
  kept: u64,
}

impl Contents for Torn<'_> {
  fn entries(&self, directory: &str, revision: Revision) -> Option<Vec<String>> {
    self.green.entries(directory, revision)
  }

  fn mergeinfo_value(&self, properties: &ContentIdentity) -> Option<&[u8]> {
    self.green.mergeinfo_value(properties)
  }
}

impl Repository for Torn<'_> {
  fn revision_properties(&self, revision: Revision) -> Option<Vec<(&[u8], &[u8])>> {
    self.green.revision_properties(revision)
  }

  fn properties(&self, identity: &ContentIdentity) -> Option<Vec<(&[u8], &[u8])>> {
    self.green.properties(identity)
  }

  fn text(&self, identity: &ContentIdentity, offset: u64) -> Option<&[u8]> {
    let left = usize::try_from(self.kept.checked_sub(offset)?).ok()?;
    let piece = self.green.text(identity, offset)?;
    if left == 0 {
      return None;
    }
    piece.get(..piece.len().min(left))
  }
}

/// A-613 part 2 (`get-file`): a text the index names and the green does not hold ends after what was read with the
/// empty string and a refusal, `Filesystem is corrupt`, as `svnserve` ends a read that fails.
#[test]
fn a_text_the_green_does_not_hold_ends_with_a_refusal() {
  let fixture = Fixture::new();
  let torn = Torn {
    green: &fixture.green,
    kept: 5_000,
  };
  let command = b"( get-file ( 13:trunk/big.txt ( 9 ) false true ) ) ";
  let (_, out, _) = streamed(&fixture, &torn, command, usize::MAX);
  assert_eq!(string_lengths(&out), [4_096, 0]);
  assert!(out.ends_with(b"0: ( failure ( ( 160004 21:Filesystem is corrupt 0: 0 ) ) ) "));
}

/// A-613 part 2 (`get-file`): its parameters as `svn_ra_svn__parse_tuple(params, "c(?r)bb?B")` reads them: a missing
/// boolean, a fifth item that is no boolean, a revision that is no list, each end the connection with nothing written.
#[test]
fn get_file_parameters_are_read_as_svn_ra_svn_parse_tuple_reads_them() {
  for malformed in [
    b"( get-file ( 11:trunk/a.txt ( ) true ) ) ".as_slice(),
    b"( get-file ( 11:trunk/a.txt ( ) true true maybe ) ) ",
    b"( get-file ( 11:trunk/a.txt ( ) true true 1 ) ) ",
    b"( get-file ( 11:trunk/a.txt ( ) 1 true ) ) ",
    b"( get-file ( 11:trunk/a.txt 2 true true ) ) ",
  ] {
    assert_eq!(
      answered(malformed),
      (Outcome::Close, Vec::new()),
      "{}",
      String::from_utf8_lossy(malformed)
    );
  }
}

/// The fixture's green with one revision's `svn:date` replaced by a value that does not parse.
struct Garbled<'green> {
  /// The green.
  green: &'green Green,
  /// The revision whose date is garbled.
  revision: Revision,
}

impl Contents for Garbled<'_> {
  fn entries(&self, directory: &str, revision: Revision) -> Option<Vec<String>> {
    self.green.entries(directory, revision)
  }

  fn mergeinfo_value(&self, properties: &ContentIdentity) -> Option<&[u8]> {
    self.green.mergeinfo_value(properties)
  }
}

impl Repository for Garbled<'_> {
  fn revision_properties(&self, revision: Revision) -> Option<Vec<(&[u8], &[u8])>> {
    let properties = self.green.revision_properties(revision)?;
    if revision != self.revision {
      return Some(properties);
    }
    let garbled: &[u8] = b"not a date";
    Some(
      properties
        .into_iter()
        .map(|(name, value)| (name, if name == b"svn:date" { garbled } else { value }))
        .collect(),
    )
  }

  fn properties(&self, identity: &ContentIdentity) -> Option<Vec<(&[u8], &[u8])>> {
    self.green.properties(identity)
  }

  fn text(&self, identity: &ContentIdentity, offset: u64) -> Option<&[u8]> {
    self.green.text(identity, offset)
  }
}

/// A-613 part 2 (`list`): a listing streams a slice at a time, each slice ending once what it wrote and the entries it
/// visited reach the room: the same bytes in a room of one byte as in one past the answer.
#[test]
fn a_listing_streams_a_slice_at_a_time_in_the_room_given() {
  let fixture = Fixture::new();
  let command = b"( list ( 0: ( ) infinity ( kind size ) ) ) ";
  let (_, narrow, narrow_slices) = streamed(&fixture, &fixture.green, command, 1);
  let (_, wide, wide_slices) = streamed(&fixture, &fixture.green, command, usize::MAX);
  assert_eq!(narrow, wide);
  assert_eq!(wide_slices, 1);
  let dirents = items(&wide).len().saturating_sub(3);
  assert!(
    narrow_slices > dirents,
    "{narrow_slices} slices, {dirents} dirents"
  );
}

/// A-613 part 2 (`list`): an empty pattern list answers at once, as `svn_repos_list` returns before any walk ("We
/// don't want the server to waste time here"): one slice even in a room of one byte, nothing but `done` and the
/// answer.
#[test]
fn an_empty_pattern_list_answers_without_a_walk() {
  let fixture = Fixture::new();
  let command = b"( list ( 0: ( ) infinity ( kind ) ( ) ) ) ";
  let (_, out, slices) = streamed(&fixture, &fixture.green, command, 1);
  assert_eq!(
    (out.as_slice(), slices),
    (
      b"( success ( ( ) 0: ) ) done ( success ( ) ) ".as_slice(),
      1
    )
  );
}

/// A-613 part 2 (`list`): `fill_dirent` parses each created revision's date, and one that does not parse refuses the
/// listing after the dirents before it, with `done` then "Bogus date"; with no field beyond the kind asked no date is
/// read and the listing is whole.
#[test]
fn a_date_that_does_not_parse_refuses_the_listing_after_done() {
  let fixture = Fixture::new();
  let garbled = Garbled {
    green: &fixture.green,
    revision: 8,
  };
  let command = b"( list ( 5:trunk ( 11 ) immediates ( kind time ) ) ) ";
  let (_, out, _) = streamed(&fixture, &garbled, command, usize::MAX);
  let out = String::from_utf8(out).unwrap();
  assert!(
    out.ends_with(
      "( 6:/trunk dir ( ) ( ) ( ) ( 27:2026-01-10T10:00:00.000000Z ) ( ) ) done \
       ( failure ( ( 125003 10:Bogus date 0: 0 ) ) ) "
    ),
    "{out}"
  );
  let command = b"( list ( 5:trunk ( 11 ) immediates ( kind ) ) ) ";
  let (_, out, _) = streamed(&fixture, &garbled, command, usize::MAX);
  assert!(out.ends_with(b"( 16:/trunk/empty.txt file ) done ( success ( ) ) "));
}

/// A-613 part 2 (`list`): its parameters as `svn_ra_svn__parse_tuple(params, "c(?r)w?l?l")`, `parse_dirent_fields`
/// and the pattern loop read them: no depth word, a depth that is a string, fields or patterns that are no list, a
/// field that is no word and a pattern that is no string each end the connection with nothing written; a pattern is
/// read to its first NUL.
#[test]
fn list_parameters_are_read_as_svnserve_reads_them() {
  for malformed in [
    b"( list ( 5:trunk ( ) ) ) ".as_slice(),
    b"( list ( 5:trunk ( ) 8:infinity ) ) ",
    b"( list ( 5:trunk ( ) infinity kind ) ) ",
    b"( list ( 5:trunk ( ) infinity ( kind ) 3:a.* ) ) ",
    b"( list ( 5:trunk ( ) infinity ( kind ) ( pattern ) ) ) ",
    b"( list ( 5:trunk ( ) infinity ( 4:kind ) ) ) ",
  ] {
    assert_eq!(
      answered(malformed),
      (Outcome::Close, Vec::new()),
      "{}",
      String::from_utf8_lossy(malformed)
    );
  }
  let fixture = Fixture::new();
  let command = b"( list ( 5:trunk ( ) immediates ( kind ) ( 7:read*\0x ) ) ) ";
  let (_, out, _) = streamed(&fixture, &fixture.green, command, usize::MAX);
  assert_eq!(
    out,
    b"( success ( ( ) 0: ) ) ( 17:/trunk/README.TXT file ) done ( success ( ) ) "
  );
}

/// A-613 part 2 (`get-location-segments`): its parameters as `svn_ra_svn__parse_tuple(params, "c(?r)(?r)(?r)")`
/// reads them, a missing or bare revision ending the connection with nothing written; and its walk streams a turn a
/// slice in a room of one byte, the same bytes as in a room past the answer.
#[test]
fn location_segments_are_read_as_svnserve_reads_them_and_stream() {
  for malformed in [
    b"( get-location-segments ( 11:trunk/a.txt ( ) ( ) ) ) ".as_slice(),
    b"( get-location-segments ( 11:trunk/a.txt 5 ( ) ( ) ) ) ",
    b"( get-location-segments ( 11:trunk/a.txt ( ) ( 3 ) 0 ) ) ",
  ] {
    assert_eq!(
      answered(malformed),
      (Outcome::Close, Vec::new()),
      "{}",
      String::from_utf8_lossy(malformed)
    );
  }
  let fixture = Fixture::new();
  let command = b"( get-location-segments ( 17:branches/b1/a.txt ( 11 ) ( 11 ) ( 0 ) ) ) ";
  let (_, narrow, narrow_slices) = streamed(&fixture, &fixture.green, command, 1);
  let (_, wide, wide_slices) = streamed(&fixture, &fixture.green, command, usize::MAX);
  assert_eq!((narrow, narrow_slices, wide_slices), (wide, 3, 1));
}

/// The bounds a hostile command is read within.
fn hostile_bounds() -> Bounds {
  Bounds {
    item_bytes: 4096,
    word_bytes: 32,
    depth: 6,
    nodes: 256,
  }
}

/// One hostile parameter: one of the shapes the commands read, or a number, or a short string of path bytes and NUL.
fn hostile_param() -> impl Strategy<Value = String> {
  let shapes = prop::sample::select(vec![
    "( ) ",
    "( 3 ) ",
    "( 18446744073709551615 ) ",
    "5:trunk ",
    "0: ",
    "true ",
    "inherited ",
    "( 5:trunk 1:x ) ",
    "27:2026-01-03T03:00:00.000000Z ",
    "11:trunk/a.txt ",
    "false ",
    "infinity ",
    "( kind size ) ",
  ]);
  (0_u8..3, shapes, any::<u64>(), "[a-z/.\\x00]{0,12}").prop_map(|(pick, shape, number, text)| {
    match pick {
      0 => shape.to_owned(),
      1 => format!("{number} "),
      _ => format!("{}:{text} ", text.len()),
    }
  })
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

  /// A-613: hostile commands, a known command's name with parameters of any shape: never a panic, and an answer is
  /// the authentication request then a success or a refusal, or a refusal alone, or nothing at all.
  #[test]
  fn hostile_commands_are_answered_or_end_the_connection(
    name in prop::sample::select(vec![
      "get-latest-rev", "get-dated-rev", "rev-proplist", "rev-prop", "check-path", "stat",
      "get-mergeinfo", "get-iprops", "get-file", "list", "get-location-segments", "no-such-command",
    ]),
    params in prop::collection::vec(hostile_param(), 0..5),
  ) {
    let command = format!("( {name} ( {}) ) ", params.concat());
    let mut parser = Parser::new(hostile_bounds());
    let Ok(Parsed::Item { value, .. }) = parser.parse(command.as_bytes()) else {
      return Ok(());
    };
    let fixture = Fixture::new();
    let mut out = Vec::new();
    let outcome = answer(&fixture.index, &fixture.green, &Session { base: "/", uuid: UUID }, &value, &mut out)
      .map_err(|refusal| TestCaseError::fail(format!("{refusal:?}")))?;
    match outcome {
      Outcome::Close | Outcome::Elsewhere => prop_assert!(out.is_empty()),
      Outcome::Answered => {
        let answers = items(&out);
        prop_assert!(!answers.is_empty() && answers.len() <= 2);
      }
      Outcome::Streaming(mut stream) => {
        let session = Session { base: "/", uuid: UUID };
        let mut slices = 0_usize;
        while stream.resume(&fixture.index, &fixture.green, &session, &mut out, 1)
          .map_err(|refusal| TestCaseError::fail(format!("{refusal:?}")))? == Progress::More {
          slices += 1;
          prop_assert!(slices < 1_024, "a stream that does not end");
        }
        prop_assert!(items(&out).len() >= 4);
      }
    }
  }
}
