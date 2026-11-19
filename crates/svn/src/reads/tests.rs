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
use std::collections::BTreeMap;

/// A transaction name of the form §4.17.4 item 3.1 gives.
const TRANSACTION: &str = "0-1z";

/// The recorder's record of the repository the transcripts were made against.
const REPOSITORY: &str = include_str!("../../fixtures/reads/repository.txt");

/// One node of the green: its kind and its property list.
type GreenNode = (NodeKind, PropertyList);

/// The green the transcripts' repository holds: every revision's tree, the property lists and the revision
/// properties.
#[derive(Debug, Default)]
struct Green {
  /// Every revision's whole tree.
  trees: Vec<BTreeMap<String, GreenNode>>,
  /// The property lists, by identity.
  lists: BTreeMap<ContentIdentity, Vec<(Vec<u8>, Vec<u8>)>>,
  /// Every revision's properties.
  revisions: Vec<Vec<(Vec<u8>, Vec<u8>)>>,
}

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
        .map(|place| place.rsplit('/').next().unwrap().to_owned())
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
    .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
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
      Text {
        identity: {
          let mut identity = [0_u8; 32];
          identity[..16].copy_from_slice(&md5);
          identity
        },
        md5: md5.try_into().unwrap(),
        sha1: sha1.try_into().unwrap(),
        size: u64::try_from(bytes.len()).unwrap(),
      }
    });
    match action {
      Action::Delete => tree.retain(|place, _| !at_or_beneath(place, &path)),
      _ if copy_from.is_some() => {
        let source = copy_from.as_ref().unwrap();
        let from = self.green.trees[usize::try_from(source.revision).unwrap()].clone();
        for (place, node) in from
          .iter()
          .filter(|(place, _)| at_or_beneath(place, &source.path))
        {
          let moved = format!("{path}{}", &place[source.path.len()..]);
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
      let pairs = !items.is_empty()
        && items.iter().all(|item| {
          matches!(item, Value::List(pair) if pair.len() == 2
            && matches!((&pair[0], &pair[1]), (Value::String(_), Value::String(_))))
        });
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
const PROPERTY_LISTS: [&str; 2] = ["rev-proplist", "get-iprops"];

/// Replays a recorded session through the handler: each client command after the handshake answered, each answer
/// `svnserve`'s, the commands in `elsewhere` (another piece's) skipped with their two items, a malformed command ending
/// the session as `svnserve` ended it; answers how many commands were answered here.
fn replay(name: &str, client: &[u8], server: &[u8], elsewhere: &[&str]) -> usize {
  let fixture = Fixture::new();
  let session = Session { base: "/" };
  let commands = items(client);
  let answers = items(server);
  let mut at = 4;
  let mut answered = 0;
  for command in commands.iter().skip(2) {
    let Value::List(parts) = command else {
      panic!("{name}: a client item that is no command");
    };
    let Some(Value::Word(word)) = parts.first() else {
      panic!("{name}: a command without its word");
    };
    let mut out = Vec::new();
    match answer(&fixture.index, &fixture.green, &session, command, &mut out).unwrap() {
      Outcome::Answered => {
        let mine = items(&out);
        let theirs = &answers[at..at + mine.len()];
        if PROPERTY_LISTS.contains(word) {
          let mine: Vec<String> = mine.iter().map(normalized).collect();
          let theirs: Vec<String> = theirs.iter().map(normalized).collect();
          assert_eq!(mine, theirs, "{name}: {word}");
        } else {
          assert_eq!(mine.as_slice(), theirs, "{name}: {word}");
        }
        at += mine.len();
        answered += 1;
      }
      Outcome::Elsewhere => {
        assert!(
          elsewhere.contains(word),
          "{name}: {word} is another piece's"
        );
        at += 2;
      }
      Outcome::Close => {
        assert_eq!(
          at,
          answers.len(),
          "{name}: svnserve wrote after the malformed {word}"
        );
        return answered;
      }
    }
  }
  assert_eq!(
    at,
    answers.len(),
    "{name}: svnserve answered more than was replayed"
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
  ];
  for ((name, client, server), commands) in sessions {
    assert_eq!(
      replay(name, client, server, &["get-lock"]),
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
    &Session { base: "/" },
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
  let read = |text: &str| parse_time(text.as_bytes());
  assert_eq!(read("1970-01-01T00:00:00.000000Z"), Some(0));
  assert_eq!(read("1970-01-01T00:00:01.000002Z"), Some(1_000_002));
  assert_eq!(
    read("2026-01-03T03:00:00.000000Z")
      .map(format_time)
      .as_deref(),
    Some("2026-01-03T03:00:00.000000Z")
  );
  assert_eq!(
    read("2026-1-3T3:0:0.5Z").map(format_time).as_deref(),
    Some("2026-01-03T03:00:00.000005Z")
  );
  assert_eq!(
    read("2026-01-32T00:00:00.000000Z")
      .map(format_time)
      .as_deref(),
    Some("2026-02-01T00:00:00.000000Z")
  );
  assert_eq!(
    read("2024-02-29T23:59:59.999999Z")
      .map(format_time)
      .as_deref(),
    Some("2024-02-29T23:59:59.999999Z")
  );
  assert_eq!(
    read("2100-03-01T00:00:00.000000Z")
      .map(format_time)
      .as_deref(),
    Some("2100-03-01T00:00:00.000000Z")
  );
  for bogus in [
    "2026-00-01T00:00:00.000000Z",
    "2026-13-01T00:00:00.000000Z",
    "1969-12-31T23:59:59.000000Z",
    "2026-01-04",
    "2026-01-04T00:00:00Z",
    " 2026-01-04T00:00:00.000000Z",
    "+2026-01-04T00:00:00.000000Z",
    "",
  ] {
    assert_eq!(read(bogus), None, "{bogus}");
  }
  let fixture = Fixture::new();
  let session = Session { base: "/" };
  let dated = |date: &str| {
    let command = format!("( get-dated-rev ( {}:{date} ) ) ", date.len());
    let value = items(command.as_bytes()).remove(0);
    let mut out = Vec::new();
    answer(&fixture.index, &fixture.green, &session, &value, &mut out).unwrap();
    String::from_utf8(out).unwrap()
  };
  assert!(dated("2026-01-01T00:00:00.000000Z").ends_with("( success ( 0 ) ) "));
  assert!(dated("2026-01-01T01:00:00.000000Z").ends_with("( success ( 1 ) ) "));
  assert!(dated("2026-01-07T07:00:00.000000Z").ends_with("( success ( 7 ) ) "));
  assert!(dated("2026-01-06T06:00:00.000001Z").ends_with("( success ( 6 ) ) "));
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

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

  /// A-613: hostile commands, a known command's name with parameters of any shape: never a panic, and an answer is
  /// the authentication request then a success or a refusal, or a refusal alone, or nothing at all.
  #[test]
  fn hostile_commands_are_answered_or_end_the_connection(
    name in prop::sample::select(vec![
      "get-latest-rev", "get-dated-rev", "rev-proplist", "rev-prop", "check-path", "stat", "get-mergeinfo",
      "get-iprops", "no-such-command",
    ]),
    params in prop::collection::vec(prop_oneof![
      Just("( ) ".to_owned()),
      Just("( 3 ) ".to_owned()),
      Just("( 18446744073709551615 ) ".to_owned()),
      Just("5:trunk ".to_owned()),
      Just("0: ".to_owned()),
      Just("true ".to_owned()),
      Just("inherited ".to_owned()),
      Just("( 5:trunk 1:x ) ".to_owned()),
      Just("27:2026-01-03T03:00:00.000000Z ".to_owned()),
      any::<u64>().prop_map(|number| format!("{number} ")),
      "[a-z/.\\x00]{0,12}".prop_map(|text| format!("{}:{text} ", text.len())),
    ], 0..5),
  ) {
    let command = format!("( {name} ( {}) ) ", params.concat());
    let mut parser = Parser::new(hostile_bounds());
    let Ok(Parsed::Item { value, .. }) = parser.parse(command.as_bytes()) else {
      return Ok(());
    };
    let fixture = Fixture::new();
    let mut out = Vec::new();
    let outcome = answer(&fixture.index, &fixture.green, &Session { base: "/" }, &value, &mut out).unwrap();
    match outcome {
      Outcome::Close | Outcome::Elsewhere => prop_assert!(out.is_empty()),
      Outcome::Answered => {
        let answers = items(&out);
        prop_assert!(!answers.is_empty() && answers.len() <= 2);
      }
    }
  }
}
