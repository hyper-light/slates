//! The commit's request and editor commands (A-617, §4.17.4 piece 7's commit): read and written back byte for byte
//! over stock Subversion 1.15.0's own commits (`crates/svn/fixtures/commit/`, recorded by its `make-fixtures.py`), each
//! file's text delta decoded whole, each refusal of a hostile item, and the oracle: `marshal.c` `vparse_tuple`
//! transcribed, which every prototype must agree with on generated items.

use std::cell::RefCell;
use std::collections::BTreeMap;

use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};

use super::*;
use crate::ra_svn::{self, Bounds, Parsed, Parser};
use crate::svndiff;

/// A stock client's bytes of one recorded commit session.
type Session = (&'static str, &'static [u8]);

/// The recorded sessions that commit, or try to: every client stream of `sessions.txt` whose session runs `commit`,
/// `import`, `mkdir` or a copy between URLs.
const SESSIONS: [Session; 17] = [
  (
    "mkdir",
    include_bytes!("../../../fixtures/commit/mkdir.1.client.bin"),
  ),
  (
    "import",
    include_bytes!("../../../fixtures/commit/import.1.client.bin"),
  ),
  (
    "edit",
    include_bytes!("../../../fixtures/commit/edit.1.client.bin"),
  ),
  (
    "edit-large",
    include_bytes!("../../../fixtures/commit/edit-large.1.client.bin"),
  ),
  (
    "propset",
    include_bytes!("../../../fixtures/commit/propset.1.client.bin"),
  ),
  (
    "propdel",
    include_bytes!("../../../fixtures/commit/propdel.1.client.bin"),
  ),
  (
    "root-prop-stale",
    include_bytes!("../../../fixtures/commit/root-prop-stale.1.client.bin"),
  ),
  (
    "root-prop",
    include_bytes!("../../../fixtures/commit/root-prop.1.client.bin"),
  ),
  (
    "add-delete",
    include_bytes!("../../../fixtures/commit/add-delete.1.client.bin"),
  ),
  (
    "replace",
    include_bytes!("../../../fixtures/commit/replace.1.client.bin"),
  ),
  (
    "copy-url",
    include_bytes!("../../../fixtures/commit/copy-url.1.client.bin"),
  ),
  (
    "copy-modify",
    include_bytes!("../../../fixtures/commit/copy-modify.1.client.bin"),
  ),
  (
    "out-of-date",
    include_bytes!("../../../fixtures/commit/out-of-date.1.client.bin"),
  ),
  (
    "locked-by-other",
    include_bytes!("../../../fixtures/commit/locked-by-other.1.client.bin"),
  ),
  (
    "commit-with-lock",
    include_bytes!("../../../fixtures/commit/commit-with-lock.1.client.bin"),
  ),
  (
    "conflict",
    include_bytes!("../../../fixtures/commit/conflict.1.client.bin"),
  ),
  (
    "race-commit",
    include_bytes!("../../../fixtures/commit/race-commit.1.client.bin"),
  ),
];

/// The longest word a stock parser reads, which bounds the recorded streams' words.
/// Format: `marshal.c` `MAX_WORD_LENGTH` (66) less one: `read_item` refuses a word that reaches its 25th byte, "Word
/// is too long" (1343-1385).
const WORD_BYTES: usize = 24;

/// The deepest list a commit session's client sends.
/// Shape: `( commit ( ... ( ( name value ) ) ) )`: the command, its parameters, the revision properties, a pair.
const DEPTH: usize = 4;

/// One item of a stream and the bytes it took, its terminating space included.
type Read<'s> = (Value<'s>, &'s [u8]);

/// Every item of `stream`, each with the bytes it took.
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
    let mut parser = Parser::new(bounds);
    match parser.parse(rest).unwrap() {
      Parsed::Item { value, consumed } => {
        read.push((value, &rest[..consumed]));
        rest = &rest[consumed..];
      }
      Parsed::NeedMore { needed } => {
        panic!("a recorded stream ends inside an item ({needed} bytes needed)")
      }
    }
  }
  read
}

/// The written bytes of `value`, as the item codec writes it.
fn written(value: &Value<'_>) -> Vec<u8> {
  let mut out = Vec::new();
  ra_svn::write(value, &mut out).unwrap();
  out
}

/// Whether `value` is the client's `commit` command.
fn is_commit(value: &Value<'_>) -> bool {
  let Value::List(parts) = value else {
    return false;
  };
  parts.first() == Some(&Value::Word("commit"))
}

/// The commit request and the editor commands that follow it in `stream`, each with the bytes it took.
fn drive(stream: &[u8]) -> (Read<'_>, Vec<Read<'_>>) {
  let all = items(stream);
  let at = all
    .iter()
    .position(|(value, _)| is_commit(value))
    .expect("a commit session holds a commit command");
  let mut rest = all.into_iter().skip(at);
  let request = rest.next().unwrap();
  (request, rest.collect())
}

/// The `commit` command's parameter list.
fn params<'v, 'a>(request: &'v Value<'a>) -> &'v [Value<'a>] {
  let Value::List(parts) = request else {
    panic!("the commit command is a list")
  };
  let Some(Value::List(params)) = parts.get(1) else {
    panic!("the commit command has a parameter list")
  };
  params
}

/// The commands `session`'s drive is, each read.
fn commands(stream: &[u8]) -> Vec<EditCommand<'_>> {
  drive(stream)
    .1
    .iter()
    .map(|(value, _)| EditCommand::read(value).unwrap())
    .collect()
}

/// Each command's name, in order.
fn names(commands: &[EditCommand<'_>]) -> Vec<&'static str> {
  commands.iter().map(EditCommand::name).collect()
}

/// Checks that `session`'s request and every command read and write back to the very bytes the client sent.
fn round_trips(name: &str, stream: &[u8], census: &mut BTreeMap<&'static str, usize>) {
  let ((request, request_bytes), drive) = drive(stream);
  let read = CommitRequest::read(params(&request)).unwrap();
  assert_eq!(
    written(&read.to_value()),
    request_bytes,
    "{name}: the request"
  );
  for (value, bytes) in &drive {
    let command = EditCommand::read(value).unwrap();
    assert_eq!(written(&command.to_value()), *bytes, "{name}: {command:?}");
    *census.entry(command.name()).or_default() += 1;
  }
  let last = drive
    .last()
    .map(|(value, _)| EditCommand::read(value).unwrap());
  assert_eq!(last, Some(EditCommand::CloseEdit), "{name}: the drive ends");
}

/// A-617 (§4.17.4 "5. The wire"; `editorp.c` and `marshal.c`'s readers and writers). Do: read every recorded stock
/// commit's request and every editor command after it, and write each back as a stock client writes it. Expect: the
/// very bytes the client sent, every drive ending at `close-edit`, and every command a commit's drive sends met.
#[test]
fn every_stock_commit_reads_and_writes_back_byte_for_byte() {
  let mut census = BTreeMap::new();
  for (name, stream) in SESSIONS {
    round_trips(name, stream, &mut census);
  }
  let met = [
    "open-root",
    "open-dir",
    "add-dir",
    "open-file",
    "add-file",
    "delete-entry",
    "change-dir-prop",
    "change-file-prop",
    "apply-textdelta",
    "textdelta-chunk",
    "textdelta-end",
    "close-file",
    "close-dir",
    "close-edit",
  ];
  let missing: Vec<&str> = met
    .into_iter()
    .filter(|name| !census.contains_key(name))
    .collect();
  assert_eq!(
    missing,
    Vec::<&str>::new(),
    "commands never met: {census:?}"
  );
}

/// The text deltas of a drive, by the file token they are for, with the path the file was opened or added at.
fn deltas<'a>(commands: &[EditCommand<'a>]) -> BTreeMap<&'a [u8], (&'a [u8], Vec<u8>)> {
  let mut paths = BTreeMap::new();
  let mut streams: BTreeMap<&[u8], (&[u8], Vec<u8>)> = BTreeMap::new();
  for command in commands {
    match *command {
      EditCommand::AddFile { path, file, .. } | EditCommand::OpenFile { path, file, .. } => {
        paths.insert(file, path);
      }
      EditCommand::ApplyTextdelta { file, .. } => {
        streams.insert(file, (paths[file], Vec::new()));
      }
      EditCommand::TextdeltaChunk { file, chunk } => {
        streams.get_mut(file).unwrap().1.extend_from_slice(chunk);
      }
      _ => {}
    }
  }
  streams
}

/// `make-fixtures.py`'s large file: `LARGE_LINE` times `LARGE_LINES`, and its edit's line in the middle and line at
/// the end.
fn large_files() -> (Vec<u8>, Vec<u8>) {
  let line = b"a line of the large file, which passes one svndiff window\n";
  let lines = 4096;
  let base = line.repeat(lines);
  let mut edited = line.repeat(lines / 2);
  edited.extend_from_slice(b"a changed line in the middle\n");
  edited.extend_from_slice(&line.repeat(lines / 2));
  edited.extend_from_slice(b"an appended line\n");
  (base, edited)
}

/// The text `stream`'s delta for `path` makes of `base`.
fn applied(stream: &[u8], path: &[u8], base: &[u8]) -> Vec<u8> {
  let commands = commands(stream);
  let deltas = deltas(&commands);
  let (_, delta) = deltas
    .values()
    .find(|(at, _)| *at == path)
    .expect("the session sends a delta for the path");
  let (version, windows) = svndiff::decode_all(delta).unwrap();
  assert_eq!(
    version,
    svndiff::Version::Two,
    "a stock client sends svndiff 2 to a server that accepts it"
  );
  svndiff::apply_all(base, &windows).unwrap()
}

/// A-617 (§4.17.4 item 3.2, "A text change"; `editorp.c` 762-826: a file's `textdelta-chunk`s are one svndiff stream).
/// Do: gather each recorded file's chunks between `apply-textdelta` and `textdelta-end`, decode every stream whole,
/// and apply five of them to the texts the recording script made. Expect: every stream decodes; the import's `a.txt`
/// and binary file (its session rooted at the trunk, so `project/...`), the edit's `a.txt`, the replacement's `c.txt`
/// and the large file's edit across three windows come out as the script wrote them.
#[test]
fn the_text_deltas_of_stock_commits_are_whole_svndiff_streams() {
  for (name, stream) in SESSIONS {
    for (file, (path, delta)) in deltas(&commands(stream)) {
      let decoded = svndiff::decode_all(&delta);
      assert!(decoded.is_ok(), "{name}: {file:?} at {path:?}: {decoded:?}");
    }
  }
  let session = |name: &str| SESSIONS.iter().find(|(at, _)| *at == name).unwrap().1;
  assert_eq!(
    applied(session("import"), b"project/a.txt", b""),
    b"alpha\nbeta\ngamma\n"
  );
  let binary: Vec<u8> = (0..=u8::MAX).collect::<Vec<u8>>().repeat(4);
  assert_eq!(
    applied(session("import"), b"project/binary.bin", b""),
    binary
  );
  assert_eq!(
    applied(session("edit"), b"a.txt", b"alpha\nbeta\ngamma\n"),
    b"alpha\nBETA\ngamma\ndelta\n"
  );
  assert_eq!(
    applied(session("replace"), b"c.txt", b""),
    b"the replacement\n"
  );
  let (base, edited) = large_files();
  assert_eq!(applied(session("edit-large"), b"large.txt", &base), edited);
}

/// The session `name`'s commands.
fn session_commands(name: &str) -> Vec<EditCommand<'static>> {
  let stream = SESSIONS.iter().find(|(at, _)| *at == name).unwrap().1;
  commands(stream)
}

/// A-617 (§4.17.4 items 3.1 and 3.2, the fields each command carries). Do: read the edit's, the stale root's and the
/// branch's drives. Expect: the edit opens the root without a revision and `a.txt` at revision 2 under it, closing the
/// root before the file's text, as a stock client orders a drive; the stale root's drive opens the root at revision 2
/// and sets `rootprop` on it; the branch adds `branches` and then `branches/b1` copied from the trunk's URL at revision
/// 9.
#[test]
fn the_drives_carry_what_svnserve_reads_from_them() {
  assert_eq!(
    names(&session_commands("edit")),
    [
      "open-root",
      "open-file",
      "close-dir",
      "apply-textdelta",
      "textdelta-chunk",
      "textdelta-chunk",
      "textdelta-chunk",
      "textdelta-chunk",
      "textdelta-end",
      "close-file",
      "close-edit",
    ]
  );
  assert_eq!(
    session_commands("edit")[..2],
    [
      EditCommand::OpenRoot {
        revision: None,
        root: b"d0",
      },
      EditCommand::OpenFile {
        path: b"a.txt",
        directory: b"d0",
        file: b"c1",
        revision: Some(2),
      },
    ]
  );
  check_stale_root_and_branch();
}

/// The stale root's and the branch's commands.
fn check_stale_root_and_branch() {
  assert_eq!(
    session_commands("root-prop-stale")[..2],
    [
      EditCommand::OpenRoot {
        revision: Some(2),
        root: b"d0",
      },
      EditCommand::ChangeDirProp {
        directory: b"d0",
        name: b"rootprop",
        value: Some(b"on the session root"),
      },
    ]
  );
  assert_eq!(
    session_commands("copy-url")[2],
    EditCommand::AddDir {
      path: b"branches/b1",
      parent: b"d1",
      child: b"d2",
      copied_from: Some(CopiedFrom {
        path: b"svn+fixture://fixture/repo/trunk",
        revision: 9,
      }),
    }
  );
}

/// The drive of session `name`, its request among it.
fn session(name: &str) -> &'static [u8] {
  SESSIONS.iter().find(|(at, _)| *at == name).unwrap().1
}

/// The names of the revision properties `request` carries, sorted.
fn property_names<'a>(request: &CommitRequest<'_, 'a>) -> Vec<&'a [u8]> {
  let mut names: Vec<&[u8]> = request
    .rest
    .as_ref()
    .and_then(|rest| rest.revision_properties.as_ref())
    .map(|properties| properties.iter().map(|&(name, _)| name).collect())
    .unwrap_or_default();
  names.sort_unstable();
  names
}

/// A-617 (§4.17.4 items 3.4 and 3.5; `serve.c` `commit`). Do: read the requests of the commit that holds bob's lock,
/// of the branch made between URLs, and of a property deleted. Expect: bob's names `a.txt` with his lock's token and
/// does not keep its locks; the branch keeps them and names none; every request carries `svn:log` and the client's
/// two transaction properties.
#[test]
fn the_requests_carry_their_locks_and_revision_properties() {
  let ((held_value, _), _) = drive(session("commit-with-lock"));
  let held = CommitRequest::read(params(&held_value)).unwrap();
  let tokens = held.lock_tokens().unwrap();
  assert_eq!(
    tokens.iter().map(|(path, _)| *path).collect::<Vec<_>>(),
    [b"a.txt"]
  );
  assert!(
    tokens
      .iter()
      .all(|(_, token)| token.starts_with(b"opaquelocktoken:"))
  );
  assert!(!held.keep_locks());
  let ((branch_value, _), _) = drive(session("copy-url"));
  let branch = CommitRequest::read(params(&branch_value)).unwrap();
  assert_eq!(
    (branch.lock_tokens(), branch.keep_locks()),
    (Ok(Vec::new()), true)
  );
  let expected: [&[u8]; 3] = [
    b"svn:log",
    b"svn:txn-client-compat-version",
    b"svn:txn-user-agent",
  ];
  for request in [&held, &branch] {
    assert_eq!(property_names(request), expected);
  }
}

/// A command item named `name` with `params`.
fn item<'a>(name: &'a str, params: Vec<Value<'a>>) -> Value<'a> {
  Value::List(vec![Value::Word(name), Value::List(params)])
}

/// The refusal of a shape that is not the prototype's.
fn malformed<T>() -> Result<T, EditRefusal> {
  Err(EditRefusal::Malformed(Malformation::Shape))
}

/// A-617 (`vparse_tuple` 1608-1718: an item missing, an item of another kind, a copy's list with its path alone). Do:
/// read `open-file` without its revision's list, with a word for its path, with a number for its revision list,
/// `add-file` with a copy list holding only a path, and `open-file` with an item past its prototype. Expect: the first
/// four refused malformed, `E210004`, ending the connection; the last read, the extra item ignored.
#[test]
fn a_command_out_of_its_prototype_is_malformed_and_items_past_it_are_ignored() {
  let string = |bytes: &'static [u8]| Value::String(bytes);
  let short = item(
    "open-file",
    vec![string(b"a"), string(b"d0"), string(b"c1")],
  );
  assert_eq!(EditCommand::read(&short), malformed());
  let word = item(
    "open-file",
    vec![
      Value::Word("a"),
      string(b"d0"),
      string(b"c1"),
      Value::List(vec![]),
    ],
  );
  assert_eq!(EditCommand::read(&word), malformed());
  let number = item(
    "open-file",
    vec![string(b"a"), string(b"d0"), string(b"c1"), Value::Number(2)],
  );
  assert_eq!(EditCommand::read(&number), malformed());
  let path_alone = item(
    "add-file",
    vec![
      string(b"a"),
      string(b"d0"),
      string(b"c1"),
      Value::List(vec![string(b"/x")]),
    ],
  );
  let refused = EditCommand::read(&path_alone).unwrap_err();
  assert_eq!(
    (refused.subversion_code(), refused.ends_connection()),
    (210_004, true)
  );
  let extra = item(
    "open-file",
    vec![
      string(b"a"),
      string(b"d0"),
      string(b"c1"),
      Value::List(vec![]),
      Value::Word("extra"),
    ],
  );
  assert_eq!(
    EditCommand::read(&extra),
    Ok(EditCommand::OpenFile {
      path: b"a",
      directory: b"d0",
      file: b"c1",
      revision: None,
    })
  );
}

/// A-617 (`editorp.c` 1127-1132: an unknown command is a command error; `read_tuple`'s `"wl"`). Do: read a command
/// named `rename-file`, an item that is a word, and a command whose parameters are a string. Expect: the first refused
/// as unknown, `E210001`, the connection kept (`svnserve` writes a failure and aborts the edit); the others malformed.
#[test]
fn an_unknown_command_is_a_command_failure_and_a_non_command_ends_the_connection() {
  let unknown = EditCommand::read(&item("rename-file", vec![])).unwrap_err();
  assert_eq!(
    unknown,
    EditRefusal::UnknownCommand {
      name: "rename-file".to_owned(),
    }
  );
  assert_eq!(
    (unknown.subversion_code(), unknown.ends_connection()),
    (210_001, false)
  );
  assert_eq!(EditCommand::read(&Value::Word("open-root")), malformed());
  let string_params = Value::List(vec![Value::Word("close-edit"), Value::String(b"x")]);
  assert_eq!(EditCommand::read(&string_params), malformed());
}

/// A-617 (module documentation, "Where this is stricter"). Do: read a `delete-entry` whose path holds a NUL byte, an
/// `open-dir` naming revision 2^63 and one naming 2^63 - 1, and a `close-dir` whose token holds a NUL byte. Expect: the
/// NUL path and the revision past `svn_revnum_t` refused malformed; 2^63 - 1 and the NUL token read, a token being any
/// bytes (`s`).
#[test]
fn a_nul_in_a_c_string_and_a_revision_past_svn_revnum_t_are_refused() {
  let nul = item(
    "delete-entry",
    vec![
      Value::String(b"a\0b"),
      Value::List(vec![]),
      Value::String(b"d0"),
    ],
  );
  assert_eq!(
    EditCommand::read(&nul),
    Err(EditRefusal::Malformed(Malformation::NulInString))
  );
  let at = |revision: u64| {
    item(
      "open-dir",
      vec![
        Value::String(b"a"),
        Value::String(b"d0"),
        Value::String(b"d1"),
        Value::List(vec![Value::Number(revision)]),
      ],
    )
  };
  let largest = i64::MAX.unsigned_abs();
  assert_eq!(
    EditCommand::read(&at(largest + 1)),
    Err(EditRefusal::Malformed(Malformation::RevisionPastRange))
  );
  assert!(EditCommand::read(&at(largest)).is_ok());
  let token = item("close-dir", vec![Value::String(b"d\0")]);
  assert_eq!(
    EditCommand::read(&token),
    Ok(EditCommand::CloseDir { directory: b"d\0" })
  );
}

/// A-617 (`serve.c` `commit` 1495-1510 and `add_lock_tokens` 1360-1409). Do: read a request of one item, one of
/// three items without revision properties, one whose keep-locks is not a boolean, one missing it, one whose
/// revision properties hold a string, and the lock tokens of requests holding a string, a pair whose path is a number,
/// a pair whose token is a word, a pair of one item and a path with a NUL. Expect: the one-item form keeps its locks
/// and names none; the three-item form has no revision properties; the next three malformed; each lock-token list
/// refused with `add_lock_tokens`' own refusal, `E210004` written as a command failure.
#[test]
fn a_commit_request_is_read_as_svnserve_reads_it() {
  let log = Value::String(b"log");
  let old = [log.clone()];
  let one = CommitRequest::read(&old).unwrap();
  assert!(one.keep_locks() && one.lock_tokens() == Ok(Vec::new()) && one.rest.is_none());
  let three = [log.clone(), Value::List(vec![]), Value::Word("false")];
  let read = CommitRequest::read(&three).unwrap();
  assert_eq!(read.rest.map(|rest| rest.revision_properties), Some(None));
  let not_boolean = [log.clone(), Value::List(vec![]), Value::Word("maybe")];
  assert_eq!(CommitRequest::read(&not_boolean), malformed());
  let missing = [log.clone(), Value::List(vec![])];
  assert_eq!(CommitRequest::read(&missing), malformed());
  let bad_properties = [
    log,
    Value::List(vec![]),
    Value::Word("true"),
    Value::List(vec![Value::String(b"x")]),
  ];
  assert_eq!(CommitRequest::read(&bad_properties), malformed());
  check_lock_tokens();
}

/// The lock tokens' refusals.
fn check_lock_tokens() {
  let pair = |items: Vec<Value<'static>>| Value::List(items);
  let cases = [
    (Value::String(b"x"), LockTokensRefusal::NotListOfLists),
    (
      pair(vec![Value::Number(1), Value::String(b"t")]),
      LockTokensRefusal::PathNotString,
    ),
    (
      pair(vec![Value::String(b"a"), Value::Word("t")]),
      LockTokensRefusal::TokenNotString,
    ),
    (
      pair(vec![Value::String(b"a")]),
      LockTokensRefusal::ShortPair,
    ),
    (
      pair(vec![Value::String(b"a\0"), Value::String(b"t")]),
      LockTokensRefusal::NulInString,
    ),
  ];
  for (token, refusal) in cases {
    let params = [
      Value::String(b"log"),
      Value::List(vec![token]),
      Value::Word("false"),
    ];
    let read = CommitRequest::read(&params).unwrap();
    assert_eq!(read.lock_tokens(), Err(refusal));
    assert_eq!(refusal.subversion_code(), 210_004);
  }
}

// ---------------------------------------------------------------------------------------------------------------------
// The oracle: `vparse_tuple` transcribed.

/// One value `vparse_tuple` hands its caller, with the prototype's letter that read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Got<'a> {
  /// A `c` or `s` string.
  Bytes(u8, &'a [u8]),
  /// An `r` or `n` number.
  Number(u8, u64),
  /// A `b` boolean.
  Boolean(bool),
  /// A `w` word.
  Word(&'a str),
  /// An `l` list, by its length.
  List(usize),
  /// What `?` left absent: `NULL` or `SVN_INVALID_REVNUM`.
  Absent,
}

/// `marshal.c` `vparse_tuple` (1608-1718), transcribed: reads `items` with `format` from `*at`, pushing what it reads.
fn vparse<'a>(
  items: &[Value<'a>],
  format: &[u8],
  at: &mut usize,
  got: &mut Vec<Got<'a>>,
) -> Result<(), ()> {
  let mut count = 0;
  while format.get(*at).is_some() && count < items.len() {
    if format.get(*at) == Some(&b'?') {
      *at += 1;
    }
    match read_one(items.get(count), format, at, got)? {
      Step::Read => {}
      Step::End => return Ok(()),
      Step::Mismatch => break,
    }
    *at += 1;
    count += 1;
  }
  if format.get(*at) == Some(&b'?') {
    return fill_absent(format, at, got);
  }
  match format.get(*at) {
    Some(&letter) if letter != b')' => Err(()),
    _ => Ok(()),
  }
}

/// What one turn of `vparse_tuple`'s loop did.
enum Step {
  /// It read the item.
  Read,
  /// It met the `)` that ends a nested prototype.
  End,
  /// The item is not the letter's kind: the loop breaks.
  Mismatch,
}

/// One turn of `vparse_tuple`'s loop: the item `element` read with the letter at `*at`.
fn read_one<'a>(
  element: Option<&Value<'a>>,
  format: &[u8],
  at: &mut usize,
  got: &mut Vec<Got<'a>>,
) -> Result<Step, ()> {
  let letter = format.get(*at).copied().unwrap_or(0);
  let read = match (letter, element) {
    (b'(', Some(Value::List(inner))) => {
      *at += 1;
      vparse(inner, format, at, got)?;
      return Ok(Step::Read);
    }
    (b'c' | b's', Some(Value::String(bytes))) => Got::Bytes(letter, bytes),
    (b'w', Some(Value::Word(word))) => Got::Word(word),
    (b'b', Some(Value::Word("true"))) => Got::Boolean(true),
    (b'b', Some(Value::Word("false"))) => Got::Boolean(false),
    (b'n' | b'r', Some(Value::Number(number))) => Got::Number(letter, *number),
    (b'l', Some(Value::List(inner))) => Got::List(inner.len()),
    (b')', _) => return Ok(Step::End),
    _ => return Ok(Step::Mismatch),
  };
  got.push(read);
  Ok(Step::Read)
}

/// `vparse_tuple`'s `?` tail: every letter left absent, up to the `)` of the tuple it is in.
fn fill_absent(format: &[u8], at: &mut usize, got: &mut Vec<Got<'_>>) -> Result<(), ()> {
  let mut nesting = 0_i32;
  while let Some(&letter) = format.get(*at) {
    match letter {
      b'?' => {}
      b'(' => nesting += 1,
      b')' => {
        nesting -= 1;
        if nesting < 0 {
          return Ok(());
        }
      }
      _ => got.push(Got::Absent),
    }
    *at += 1;
  }
  Ok(())
}

/// Each editor command's prototype, as its handler reads it (`editorp.c` 532-935); the commands that read no
/// parameters have none.
const PROTOTYPES: [(&str, &str); 20] = [
  ("target-rev", "r"),
  ("open-root", "(?r)s"),
  ("delete-entry", "c(?r)s"),
  ("add-dir", "css(?cr)"),
  ("open-dir", "css(?r)"),
  ("change-dir-prop", "sc(?s)"),
  ("close-dir", "s"),
  ("absent-dir", "cs"),
  ("add-file", "css(?cr)"),
  ("open-file", "css(?r)"),
  ("apply-textdelta", "s(?c)"),
  ("textdelta-chunk", "ss"),
  ("textdelta-end", "s"),
  ("change-file-prop", "sc(?s)"),
  ("close-file", "s(?c)"),
  ("absent-file", "cs"),
  ("close-edit", ""),
  ("abort-edit", ""),
  ("finish-replay", ""),
  ("failure", ""),
];

/// What the typed reader read, as `vparse_tuple` hands it, in the prototype's order.
fn got_of<'a>(command: &EditCommand<'a>) -> Vec<Got<'a>> {
  let c = |bytes| Got::Bytes(b'c', bytes);
  let s = |bytes| Got::Bytes(b's', bytes);
  let r = |revision: Option<u64>| revision.map_or(Got::Absent, |number| Got::Number(b'r', number));
  let o =
    |bytes: Option<&'a [u8]>, letter| bytes.map_or(Got::Absent, |bytes| Got::Bytes(letter, bytes));
  let copy = |copied: Option<CopiedFrom<'a>>| match copied {
    Some(source) => vec![c(source.path), Got::Number(b'r', source.revision)],
    None => vec![Got::Absent, Got::Absent],
  };
  match *command {
    EditCommand::TargetRev { revision } => vec![Got::Number(b'r', revision)],
    EditCommand::OpenRoot { revision, root } => vec![r(revision), s(root)],
    EditCommand::DeleteEntry {
      path,
      revision,
      directory,
    } => vec![c(path), r(revision), s(directory)],
    EditCommand::AddDir {
      path,
      parent,
      child,
      copied_from,
    }
    | EditCommand::AddFile {
      path,
      directory: parent,
      file: child,
      copied_from,
    } => [vec![c(path), s(parent), s(child)], copy(copied_from)].concat(),
    EditCommand::OpenDir {
      path,
      parent,
      child,
      revision,
    }
    | EditCommand::OpenFile {
      path,
      directory: parent,
      file: child,
      revision,
    } => vec![c(path), s(parent), s(child), r(revision)],
    EditCommand::ChangeDirProp {
      directory: token,
      name,
      value,
    }
    | EditCommand::ChangeFileProp {
      file: token,
      name,
      value,
    } => vec![s(token), c(name), o(value, b's')],
    EditCommand::CloseDir { directory: token } | EditCommand::TextdeltaEnd { file: token } => {
      vec![s(token)]
    }
    EditCommand::AbsentDir { path, parent } | EditCommand::AbsentFile { path, parent } => {
      vec![c(path), s(parent)]
    }
    EditCommand::ApplyTextdelta {
      file,
      base_checksum: checksum,
    }
    | EditCommand::CloseFile {
      file,
      text_checksum: checksum,
    } => vec![s(file), o(checksum, b'c')],
    EditCommand::TextdeltaChunk { file, chunk } => vec![s(file), s(chunk)],
    EditCommand::CloseEdit
    | EditCommand::AbortEdit
    | EditCommand::FinishReplay
    | EditCommand::Failure => Vec::new(),
  }
}

/// An item a generated case owns, from which the borrowed [`Value`] is made.
#[derive(Clone, Debug)]
enum Owned {
  Word(&'static str),
  Number(u64),
  Bytes(Vec<u8>),
  List(Vec<Owned>),
}

impl Owned {
  /// The item as the item codec reads it.
  fn value(&self) -> Value<'_> {
    match self {
      Self::Word(word) => Value::Word(word),
      Self::Number(number) => Value::Number(*number),
      Self::Bytes(bytes) => Value::String(bytes),
      Self::List(items) => Value::List(items.iter().map(Owned::value).collect()),
    }
  }
}

/// The words a generated item may be: booleans and others.
const WORDS: [&str; 4] = ["true", "false", "maybe", "open-root"];

/// The numbers a generated item may be: small, at and past `svn_revnum_t`'s end, and the largest.
const NUMBERS: [u64; 5] = [
  0,
  7,
  9_223_372_036_854_775_807,
  9_223_372_036_854_775_808,
  u64::MAX,
];

/// The strings a generated item may be: plain, empty, and holding a NUL byte.
const STRINGS: [&[u8]; 4] = [b"a.txt", b"", b"d\0", b"svn+fixture://fixture/repo/trunk"];

/// The next generated pick, or zero when the picks run out.
fn pick(picks: &mut std::slice::Iter<'_, u8>) -> u8 {
  picks.next().copied().unwrap_or(0)
}

/// An item of any kind, from `picks`.
fn any_item(picks: &mut std::slice::Iter<'_, u8>, depth: u8) -> Owned {
  let choice = pick(picks);
  match choice % 4 {
    0 => Owned::Word(WORDS[usize::from(pick(picks)) % WORDS.len()]),
    1 => Owned::Number(NUMBERS[usize::from(pick(picks)) % NUMBERS.len()]),
    2 => Owned::Bytes(STRINGS[usize::from(pick(picks)) % STRINGS.len()].to_vec()),
    _ if depth == 0 => Owned::List(Vec::new()),
    _ => {
      let length = pick(picks) % 3;
      Owned::List((0..length).map(|_| any_item(picks, depth - 1)).collect())
    }
  }
}

/// An item of the kind `letter` names.
fn item_of(letter: u8, picks: &mut std::slice::Iter<'_, u8>) -> Owned {
  match letter {
    b'r' | b'n' => Owned::Number(NUMBERS[usize::from(pick(picks)) % NUMBERS.len()]),
    b'b' => Owned::Word(WORDS[usize::from(pick(picks)) % 2]),
    b'w' => Owned::Word(WORDS[usize::from(pick(picks)) % WORDS.len()]),
    b'l' => Owned::List(Vec::new()),
    _ => Owned::Bytes(STRINGS[usize::from(pick(picks)) % STRINGS.len()].to_vec()),
  }
}

/// Parameters for `format` from `picks`, mostly of the prototype's shape: each item of its letter's kind, or of any
/// kind, or the list ending early, and items past the prototype.
fn params_for(format: &[u8], picks: &mut std::slice::Iter<'_, u8>) -> Vec<Owned> {
  let mut items = Vec::new();
  let mut at = 0;
  while let Some(&letter) = format.get(at) {
    at += 1;
    match letter {
      b'?' => {}
      b')' => break,
      b'(' => {
        let inner_start = at;
        let depth = format[inner_start..]
          .iter()
          .position(|&l| l == b')')
          .unwrap_or(0);
        let inner = &format[inner_start..inner_start + depth];
        at = inner_start + depth + 1;
        let empty = pick(picks).is_multiple_of(3);
        items.push(Owned::List(if empty {
          Vec::new()
        } else {
          params_for(inner, picks)
        }));
      }
      _ => match pick(picks) % 8 {
        0 => return items,
        1 => items.push(any_item(picks, 2)),
        _ => items.push(item_of(letter, picks)),
      },
    }
  }
  let extra = pick(picks) % 4;
  for _ in 0..extra {
    if pick(picks).is_multiple_of(2) {
      items.push(any_item(picks, 1));
    }
  }
  items
}

/// How the generated cases came out, the oracle's evidence that each outcome was met.
#[derive(Debug, Default)]
struct Census {
  /// Commands both read.
  read: usize,
  /// Commands both refused.
  refused: usize,
  /// Commands `vparse_tuple` read and the reader refused as stricter: a NUL in a C string, a revision past range.
  stricter: usize,
}

/// Whether what `vparse_tuple` read holds what the reader refuses as stricter than `svnserve`.
fn stricter(got: &[Got<'_>]) -> bool {
  got.iter().any(|value| match value {
    Got::Bytes(b'c', bytes) => bytes.contains(&0),
    Got::Number(b'r', number) => *number > i64::MAX.unsigned_abs(),
    _ => false,
  })
}

/// One generated case: the command `name` with parameters made from `picks`, read by both.
fn check_case(name: &str, picks: &[u8], census: &mut Census) -> Result<(), String> {
  let format = PROTOTYPES
    .iter()
    .find(|(command, _)| *command == name)
    .map_or(b"".as_slice(), |(_, format)| format.as_bytes());
  let params = params_for(format, &mut picks.iter());
  let params: Vec<Value<'_>> = params.iter().map(Owned::value).collect();
  let mut got = Vec::new();
  let transcribed = vparse(&params, format, &mut 0, &mut got);
  let typed = EditCommand::read(&item(name, params.clone()));
  match (transcribed, typed) {
    (Err(()), Err(EditRefusal::Malformed(_))) => census.refused += 1,
    (
      Ok(()),
      Err(EditRefusal::Malformed(Malformation::NulInString | Malformation::RevisionPastRange)),
    ) if stricter(&got) => {
      census.stricter += 1;
    }
    (Ok(()), Ok(command)) if !stricter(&got) && got_of(&command) == got => census.read += 1,
    (transcribed, typed) => {
      return Err(format!(
        "{name} {params:?}: vparse_tuple {transcribed:?} {got:?}, the reader {typed:?}"
      ));
    }
  }
  Ok(())
}

/// A-617 (`marshal.c` `vparse_tuple`, the oracle). Do: over generated parameters for every editor command, mostly of
/// the prototype's shape with items of other kinds, lists ended early, empty optional lists and items past the
/// prototype among them, read each by `vparse_tuple` transcribed and by the typed reader. Expect: both read the same
/// values or both refuse, the reader refusing alone only what it refuses as stricter (a NUL in a C string, a revision
/// past `svn_revnum_t`); and the census, printed every run, showing all three outcomes.
#[test]
#[allow(
  clippy::print_stderr,
  reason = "the census is the oracle's evidence, printed with every run"
)]
fn every_prototype_reads_as_vparse_tuple_reads_it() {
  let census = RefCell::new(Census::default());
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(Config::default()));
  let result = runner.run(
    &(
      0..PROTOTYPES.len(),
      prop::collection::vec(any::<u8>(), 0..48),
    ),
    |(index, picks)| {
      let name = PROTOTYPES[index].0;
      check_case(name, &picks, &mut census.borrow_mut()).map_err(TestCaseError::fail)
    },
  );
  if let Err(failure) = result {
    panic!("{failure}");
  }
  let census = census.into_inner();
  eprintln!("prototype census: {census:?}");
  assert!(
    [census.read, census.refused, census.stricter]
      .iter()
      .all(|&count| count > 0),
    "an outcome never met: {census:?}"
  );
}
