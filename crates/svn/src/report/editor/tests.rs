//! The editor writer's tests (A-616, part 2; §4.17.4 "5. The wire").
//!
//! The oracle is `svnserve` itself: every drive stock Subversion 1.15.0's `svnserve` wrote in the
//! recorded sessions (`crates/svn/fixtures/report/`, each file's SHA-256 in `SHA256SUMS`) is read
//! back into calls, its deltas decoded into windows, and written again. The writer must write the
//! same calls, and every item byte for byte, a chunk of a delta `svnserve` stored LZ4-compressed
//! aside, whose windows must decode alike. Beside them: absent entries to a client that did not
//! announce them, svndiff version 0 to a client that does not accept version 2, every call out of
//! the editor's order refused with nothing written, the bound on nodes open, and hostile window
//! encodings.

use super::*;
use crate::report::drive::Facts;
use crate::report::recorded::{SESSIONS, command, items, stock_capabilities};
use crate::svndiff::decode_all;

/// The most nodes the tests' writers hold open: past the fixtures' deepest path.
const MOST_OPEN: usize = 64;

/// The editor commands `svnserve`'s drive writes (`marshal.c` `svn_ra_svn__write_cmd_*`).
const EDITOR_COMMANDS: &[&str] = &[
  "target-rev",
  "open-root",
  "delete-entry",
  "add-dir",
  "open-dir",
  "change-dir-prop",
  "close-dir",
  "absent-dir",
  "add-file",
  "open-file",
  "apply-textdelta",
  "textdelta-chunk",
  "textdelta-end",
  "change-file-prop",
  "close-file",
  "absent-file",
  "close-edit",
  "abort-edit",
];

/// The drives the recordings hold, which the replay test must meet: the 22 stock checkouts,
/// updates, switches, `status -u` and diff, and six hand-made sessions' (an unknown report command,
/// a missing path, a revision past the youngest, an absent target, the early answer and the refused
/// edit).
const RECORDED_DRIVES: usize = 28;

/// One drive call as a recording holds it, owned; a delta's chunks decoded into windows.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Written {
  /// `target-rev`.
  TargetRevision(u64),
  /// `open-root`.
  OpenRoot(Option<u64>),
  /// `open-dir` or `open-file`.
  Open(NodeKind, String, Option<u64>),
  /// `add-dir` or `add-file`.
  Add(NodeKind, String, Option<(String, u64)>),
  /// `delete-entry`.
  Delete(String, Option<u64>),
  /// `absent-dir` or `absent-file`.
  Absent(NodeKind, String),
  /// `change-dir-prop` or `change-file-prop`.
  ChangeProperty(String, Option<Vec<u8>>),
  /// `apply-textdelta`, its chunks and `textdelta-end`: the base MD5, the stream's version byte and
  /// its windows.
  Delta(Option<[u8; 16]>, u8, Vec<Window>),
  /// `close-file`.
  CloseFile([u8; 16]),
  /// `close-dir`.
  CloseDirectory,
  /// `close-edit`.
  CloseEdit,
  /// `abort-edit`.
  AbortEdit,
}

/// A drive read back: its calls, and for each of its items whether it is a chunk of a delta whose
/// recorded sections hold an LZ4 block.
struct ReadDrive {
  /// The calls.
  calls: Vec<Written>,
  /// Per item, whether its bytes may differ from ours.
  compressed: Vec<bool>,
}

/// The items of `server` that are the drive: from its first editor command while they are editor
/// commands.
fn drive_items(server: &[u8]) -> Vec<&[u8]> {
  items(server)
    .into_iter()
    .skip_while(|item| !is_editor_command(item))
    .take_while(|item| is_editor_command(item))
    .collect()
}

/// Whether an item is one of the drive's commands.
fn is_editor_command(item: &[u8]) -> bool {
  command(item).is_some_and(|(name, _)| EDITOR_COMMANDS.contains(&name.as_str()))
}

/// A string parameter.
fn text(value: &Value<'_>) -> String {
  match value {
    Value::String(bytes) => String::from_utf8(bytes.to_vec()).unwrap(),
    other => panic!("a string, not {other:?}"),
  }
}

/// A list's items.
fn list_items<'v>(value: &'v Value<'v>) -> &'v [Value<'v>] {
  match value {
    Value::List(items) => items,
    other => panic!("a list, not {other:?}"),
  }
}

/// A number parameter.
fn number_of(value: &Value<'_>) -> u64 {
  match value {
    Value::Number(number) => *number,
    other => panic!("a number, not {other:?}"),
  }
}

/// A string parameter's bytes.
fn bytes_of(value: &Value<'_>) -> Vec<u8> {
  match value {
    Value::String(bytes) => bytes.to_vec(),
    other => panic!("a string, not {other:?}"),
  }
}

/// An optional revision's list, `( )` or `( revision )`.
fn optional_number(value: &Value<'_>) -> Option<u64> {
  list_items(value).first().map(number_of)
}

/// An optional digest's list, in hexadecimal.
fn optional_digest(value: &Value<'_>) -> Option<[u8; 16]> {
  list_items(value).first().map(|item| digest(&text(item)))
}

/// An optional string's list.
fn optional_bytes(value: &Value<'_>) -> Option<Vec<u8>> {
  list_items(value).first().map(bytes_of)
}

/// A hexadecimal MD5 digest.
fn digest(hex: &str) -> [u8; 16] {
  let mut bytes = [0u8; 16];
  for (slot, at) in bytes.iter_mut().zip((0..hex.len()).step_by(2)) {
    *slot = u8::from_str_radix(hex.get(at..at + 2).unwrap(), 16).unwrap();
  }
  bytes
}

/// The kind a command's word names.
fn kind_of(name: &str) -> NodeKind {
  if name.ends_with("-dir") {
    NodeKind::Directory
  } else {
    NodeKind::File
  }
}

/// Reads a drive's items back into calls, checking that every command names the innermost open
/// node or its directory, as the writer assumes.
fn read_drive(drive: &[&[u8]]) -> ReadDrive {
  let mut reader = DriveReader::default();
  for item in drive {
    let (name, params) = command(item).unwrap();
    reader.command(&name, &params, item);
  }
  assert!(reader.delta.is_none(), "a delta left open");
  ReadDrive {
    calls: reader.calls,
    compressed: reader.compressed,
  }
}

/// The state of reading one drive.
#[derive(Default)]
struct DriveReader {
  /// The calls read.
  calls: Vec<Written>,
  /// Per item, whether its bytes may differ from ours.
  compressed: Vec<bool>,
  /// The tokens of the nodes open, the innermost last.
  open: Vec<String>,
  /// The delta being read: its base MD5, its chunks and the items they came in.
  delta: Option<(Option<[u8; 16]>, Vec<u8>, Vec<usize>)>,
}

impl DriveReader {
  /// Reads one command.
  fn command(&mut self, name: &str, params: &[Value<'_>], item: &[u8]) {
    self.compressed.push(false);
    match name {
      "textdelta-chunk" | "textdelta-end" | "apply-textdelta" => self.delta_command(name, params),
      "close-file" | "close-dir" | "close-edit" | "abort-edit" => self.close(name, params),
      _ => self.node_command(name, params, item),
    }
  }

  /// A command that opens, adds, deletes, marks absent or changes a node.
  fn node_command(&mut self, name: &str, params: &[Value<'_>], item: &[u8]) {
    let call = match (name, params) {
      ("target-rev", [Value::Number(revision)]) => Written::TargetRevision(*revision),
      ("open-root", [revision, token]) => {
        self.open.push(text(token));
        Written::OpenRoot(optional_number(revision))
      }
      ("open-dir" | "open-file", [path, parent, token, revision]) => {
        self.expect_innermost(parent);
        self.open.push(text(token));
        Written::Open(kind_of(name), text(path), optional_number(revision))
      }
      ("add-dir" | "add-file", [path, parent, token, Value::List(source)]) => {
        self.expect_innermost(parent);
        self.open.push(text(token));
        let copy_from = match source.as_slice() {
          [] => None,
          [source, Value::Number(revision)] => Some((text(source), *revision)),
          other => panic!("a copy source, not {other:?}"),
        };
        Written::Add(kind_of(name), text(path), copy_from)
      }
      ("delete-entry", [path, revision, parent]) => {
        self.expect_innermost(parent);
        Written::Delete(text(path), optional_number(revision))
      }
      ("absent-dir" | "absent-file", [path, parent]) => {
        self.expect_innermost(parent);
        Written::Absent(kind_of(name), text(path))
      }
      ("change-dir-prop" | "change-file-prop", [token, property, value]) => {
        self.expect_innermost(token);
        Written::ChangeProperty(text(property), optional_bytes(value))
      }
      _ => panic!("not a drive command: {}", String::from_utf8_lossy(item)),
    };
    self.calls.push(call);
  }

  /// `apply-textdelta`, `textdelta-chunk` or `textdelta-end`.
  fn delta_command(&mut self, name: &str, params: &[Value<'_>]) {
    let token = params.first().unwrap();
    self.expect_innermost(token);
    let at = self.compressed.len() - 1;
    match (name, params) {
      ("apply-textdelta", [_, base]) => {
        assert!(self.delta.is_none(), "a delta inside a delta");
        self.delta = Some((optional_digest(base), Vec::new(), Vec::new()));
      }
      ("textdelta-chunk", [_, Value::String(chunk)]) => {
        let (_, stream, chunks) = self.delta.as_mut().expect("a chunk outside a delta");
        stream.extend_from_slice(chunk);
        chunks.push(at);
      }
      ("textdelta-end", [_]) => self.end_delta(),
      _ => panic!("{name} out of a delta's order"),
    }
  }

  /// `textdelta-end`: the delta's stream decoded, and its chunks marked when `svnserve` compressed
  /// any of its sections.
  fn end_delta(&mut self) {
    let (base, stream, chunks) = self.delta.take().unwrap();
    let (_, windows) = decode_all(&stream).unwrap();
    if holds_lz4_block(&stream) {
      for at in chunks {
        self.compressed[at] = true;
      }
    }
    self.calls.push(Written::Delta(base, stream[3], windows));
  }

  /// `close-file`, `close-dir`, `close-edit` or `abort-edit`.
  fn close(&mut self, name: &str, params: &[Value<'_>]) {
    let call = match (name, params) {
      ("close-file", [token, checksum]) => {
        self.expect_innermost(token);
        self.open.pop();
        Written::CloseFile(optional_digest(checksum).unwrap())
      }
      ("close-dir", [token]) => {
        self.expect_innermost(token);
        self.open.pop();
        Written::CloseDirectory
      }
      ("close-edit", []) => Written::CloseEdit,
      ("abort-edit", []) => Written::AbortEdit,
      _ => panic!("{name} with parameters {params:?}"),
    };
    self.calls.push(call);
  }

  /// Checks that a command names the innermost open node.
  fn expect_innermost(&self, token: &Value<'_>) {
    assert_eq!(self.open.last(), Some(&text(token)), "not the innermost node");
  }
}

/// Whether a version-2 stream holds a section stored as an LZ4 block: a section whose bytes after
/// its original length are fewer than that length.
fn holds_lz4_block(stream: &[u8]) -> bool {
  if stream[3] != 2 {
    return false;
  }
  let mut rest = &stream[4..];
  let mut compressed = false;
  while !rest.is_empty() {
    let mut lengths = [0u64; WINDOW_HEADER_INTEGERS];
    for slot in &mut lengths {
      let (value, used) = read_integer(rest).unwrap();
      *slot = value;
      rest = &rest[used..];
    }
    let [_, _, _, instructions, new_data] = lengths.map(|length| usize::try_from(length).unwrap());
    for section in [&rest[..instructions], &rest[instructions..instructions + new_data]] {
      let (original, used) = read_integer(section).unwrap();
      compressed |= u64::try_from(section.len() - used).unwrap() < original;
    }
    rest = &rest[instructions + new_data..];
  }
  compressed
}

/// The calls a read drive makes, borrowing its windows. A node's facts are not on the wire, and
/// the writer takes none of them.
fn calls(written: &[Written]) -> Vec<Call<'_>> {
  let facts = Facts {
    repository_path: "",
    created_revision: 0,
    sha1: None,
  };
  let mut calls = Vec::new();
  for call in written {
    match call {
      Written::TargetRevision(revision) => calls.push(Call::TargetRevision(*revision)),
      Written::OpenRoot(base) => calls.push(Call::OpenRoot {
        base: *base,
        node: facts,
      }),
      Written::Open(kind, path, base) => calls.push(Call::Open {
        kind: *kind,
        path,
        base: *base,
        node: facts,
      }),
      Written::Add(kind, path, copy_from) => calls.push(Call::Add {
        kind: *kind,
        path,
        copy_from: copy_from
          .as_ref()
          .map(|(source, revision)| (source.as_str(), *revision)),
        node: facts,
      }),
      other => calls.extend(leaf_calls(other)),
    }
  }
  calls
}

/// The calls of a read call that opens no node.
fn leaf_calls(written: &Written) -> Vec<Call<'_>> {
  match written {
    Written::Delete(path, revision) => vec![Call::Delete {
      path,
      revision: *revision,
    }],
    Written::Absent(kind, path) => vec![Call::Absent { kind: *kind, path }],
    Written::ChangeProperty(name, value) => vec![Call::ChangeProperty {
      name,
      value: value.as_deref(),
    }],
    Written::Delta(base_md5, _, windows) => {
      let mut delta = vec![Call::ApplyTextDelta {
        base_md5: *base_md5,
      }];
      delta.extend(windows.iter().map(Call::Window));
      delta.push(Call::TextDeltaEnd);
      delta
    }
    Written::CloseFile(md5) => vec![Call::CloseFile { md5: *md5 }],
    Written::CloseDirectory => vec![Call::CloseDirectory],
    Written::CloseEdit => vec![Call::CloseEdit],
    Written::AbortEdit => vec![Call::AbortEdit],
    opening => panic!("{opening:?} opens a node"),
  }
}

/// Writes `calls` through a fresh writer for a client with `capabilities`.
fn write_all(calls: &[Call<'_>], capabilities: &ClientCapabilities) -> Vec<u8> {
  let mut writer = EditorWriter::new(capabilities, MOST_OPEN);
  let mut out = Vec::new();
  for call in calls {
    writer.write(call, &mut out).unwrap();
  }
  out
}

/// A-616 part 2 (`editorp.c` and `marshal.c`'s editor commands): read every recorded drive back
/// into calls and write them again for a stock 1.15 client; expect the same calls, the same
/// windows, and every item `svnserve`'s byte for byte, a chunk of a delta it stored LZ4-compressed
/// aside; and every one of the recordings' drives met.
#[test]
fn every_recorded_drive_is_written_again_as_svnserve_wrote_it() {
  let mut drives = 0;
  for session in SESSIONS {
    let theirs = drive_items(session.server);
    if theirs.is_empty() {
      continue;
    }
    drives += 1;
    let read = read_drive(&theirs);
    let written = write_all(&calls(&read.calls), &stock_capabilities());
    let ours = items(&written);
    assert_eq!(read_drive(&ours).calls, read.calls, "{}: the calls", session.name);
    assert_eq!(ours.len(), theirs.len(), "{}: the items", session.name);
    for ((ours, theirs), compressed) in ours.iter().zip(&theirs).zip(&read.compressed) {
      if !compressed {
        assert_eq!(
          String::from_utf8_lossy(ours),
          String::from_utf8_lossy(theirs),
          "{}",
          session.name
        );
      }
    }
  }
  assert_eq!(drives, RECORDED_DRIVES);
}

/// A-616 part 2 (`ra_svn_absent_dir` and `ra_svn_absent_file` skip a client without
/// `absent-entries`): write a drive holding an absent directory and an absent file for a client
/// with the capability and one without; expect both commands, then neither, the rest alike.
#[test]
fn absent_entries_go_only_to_a_client_that_announced_them() {
  let facts = Facts {
    repository_path: "/trunk",
    created_revision: 1,
    sha1: None,
  };
  let drive = [
    Call::TargetRevision(1),
    Call::OpenRoot {
      base: Some(1),
      node: facts,
    },
    Call::Absent {
      kind: NodeKind::Directory,
      path: "secret",
    },
    Call::Absent {
      kind: NodeKind::File,
      path: "hidden.txt",
    },
    Call::CloseDirectory,
    Call::CloseEdit,
  ];
  let mut without = stock_capabilities();
  without.absent_entries = false;
  let common = "( target-rev ( 1 ) ) ( open-root ( ( 1 ) 2:d0 ) ) ";
  let end = "( close-dir ( 2:d0 ) ) ( close-edit ( ) ) ";
  let absent = "( absent-dir ( 6:secret 2:d0 ) ) ( absent-file ( 10:hidden.txt 2:d0 ) ) ";
  assert_eq!(
    String::from_utf8(write_all(&drive, &stock_capabilities())).unwrap(),
    format!("{common}{absent}{end}")
  );
  assert_eq!(
    String::from_utf8(write_all(&drive, &without)).unwrap(),
    format!("{common}{end}")
  );
}

/// A-616 part 2 (`svn_ra_svn__svndiff_version`: version 2 only to a client that accepts it, never
/// version 1): write a recorded checkout's drive for a client that announced `svndiff1` but not
/// `accepts-svndiff2`; expect every delta's stream in version 0 and its windows alike.
#[test]
fn a_client_that_does_not_accept_svndiff2_is_sent_version_0() {
  let checkout = SESSIONS.iter().find(|session| session.name == "checkout-head.1");
  let read = read_drive(&drive_items(checkout.unwrap().server));
  let mut older = stock_capabilities();
  older.accepts_svndiff2 = false;
  let written = write_all(&calls(&read.calls), &older);
  let again = read_drive(&items(&written)).calls;
  let mut deltas = 0;
  for (ours, theirs) in again.iter().zip(&read.calls) {
    match (ours, theirs) {
      (Written::Delta(base, 0, windows), Written::Delta(their_base, 2, their_windows)) => {
        assert_eq!((base, windows), (their_base, their_windows));
        deltas += 1;
      }
      _ => assert_eq!(ours, theirs),
    }
  }
  assert_eq!(again.len(), read.calls.len());
  assert!(deltas > 0, "the checkout holds deltas");
}

/// A call and whether the writer must take it, in a sequence a test writes.
type Step<'a> = (Call<'a>, bool);

/// The facts every test call carries.
const TEST_FACTS: Facts<'static> = Facts {
  repository_path: "/trunk",
  created_revision: 1,
  sha1: None,
};

/// A window of three new bytes.
fn three_bytes() -> Window {
  Window {
    source_offset: 0,
    source_len: 0,
    target_len: 3,
    instructions: vec![crate::svndiff::Instruction::New { length: 3 }],
    new_data: b"abc".to_vec(),
  }
}

/// Writes `steps` through a writer for a stock client, each expected taken or refused out of
/// order; a refused step must append nothing and leave the writer as a writer that never saw it.
fn expect_steps(name: &str, steps: &[Step<'_>]) {
  let mut writer = EditorWriter::new(&stock_capabilities(), MOST_OPEN);
  let mut clean = EditorWriter::new(&stock_capabilities(), MOST_OPEN);
  let (mut out, mut clean_out) = (Vec::new(), Vec::new());
  for (index, (call, taken)) in steps.iter().enumerate() {
    let before = out.len();
    let result = writer.write(call, &mut out);
    if *taken {
      assert_eq!(result, Ok(()), "{name}: step {index}");
      clean.write(call, &mut clean_out).unwrap();
    } else {
      assert_eq!(result, Err(EditorRefused::OutOfOrder), "{name}: step {index}");
      assert_eq!(out.len(), before, "{name}: step {index} appended");
    }
  }
  assert_eq!(out, clean_out, "{name}: a refusal changed the writer");
}

/// The calls that open a root and a file inside it.
fn root_and_file<'a>() -> Vec<Step<'a>> {
  vec![
    (Call::TargetRevision(1), true),
    (
      Call::OpenRoot {
        base: Some(1),
        node: TEST_FACTS,
      },
      true,
    ),
    (
      Call::Add {
        kind: NodeKind::File,
        path: "a.txt",
        copy_from: None,
        node: TEST_FACTS,
      },
      true,
    ),
  ]
}

/// A-616 part 2 (the editor's order, `svn_delta.h`): write sequences that break it, each followed
/// by calls that keep it; expect each breaking call refused out of order with nothing written and
/// the writer as if it never came.
#[test]
fn a_call_out_of_the_editors_order_is_refused_and_changes_nothing() {
  let window = three_bytes();
  let md5 = [0u8; 16];
  let mut inside_a_file = root_and_file();
  inside_a_file.extend([
    (Call::CloseDirectory, false),
    (
      Call::Open {
        kind: NodeKind::File,
        path: "b.txt",
        base: Some(1),
        node: TEST_FACTS,
      },
      false,
    ),
    (Call::Window(&window), false),
    (Call::TextDeltaEnd, false),
    (Call::CloseEdit, false),
    (Call::ApplyTextDelta { base_md5: None }, true),
    (
      Call::ChangeProperty {
        name: "color",
        value: Some(b"red"),
      },
      false,
    ),
    (Call::CloseFile { md5 }, false),
    (Call::Window(&window), true),
    (Call::TextDeltaEnd, true),
    (Call::TextDeltaEnd, false),
    (Call::CloseFile { md5 }, true),
    (Call::CloseFile { md5 }, false),
    (Call::CloseDirectory, true),
    (Call::CloseDirectory, false),
    (Call::CloseEdit, true),
    (Call::CloseEdit, false),
    (Call::AbortEdit, true),
    (Call::AbortEdit, false),
    (Call::TargetRevision(1), false),
  ]);
  expect_steps("inside a file", &inside_a_file);
  expect_steps(
    "before the root",
    &[
      (Call::Window(&window), false),
      (Call::CloseDirectory, false),
      (Call::TargetRevision(2), true),
      (Call::TargetRevision(2), false),
      (
        Call::OpenRoot {
          base: None,
          node: TEST_FACTS,
        },
        true,
      ),
      (
        Call::OpenRoot {
          base: None,
          node: TEST_FACTS,
        },
        false,
      ),
      (Call::AbortEdit, true),
      (Call::CloseEdit, false),
    ],
  );
}

/// A-616 part 2 (the writer's bound on nodes open): open directories one inside another past the
/// bound; expect the one past it refused, nothing written, and the writer still taking a close.
#[test]
fn a_node_past_the_bound_on_nodes_open_is_refused() {
  fn open(path: &str) -> Call<'_> {
    Call::Add {
      kind: NodeKind::Directory,
      path,
      copy_from: None,
      node: TEST_FACTS,
    }
  }
  let mut writer = EditorWriter::new(&stock_capabilities(), 2);
  let mut out = Vec::new();
  let root = Call::OpenRoot {
    base: Some(1),
    node: TEST_FACTS,
  };
  writer.write(&root, &mut out).unwrap();
  writer.write(&open("a"), &mut out).unwrap();
  let before = out.len();
  assert_eq!(writer.write(&open("a/b"), &mut out), Err(EditorRefused::TooDeep));
  assert_eq!(out.len(), before);
  writer.write(&Call::CloseDirectory, &mut out).unwrap();
  writer.write(&open("c"), &mut out).unwrap();
  assert!(String::from_utf8(out).unwrap().ends_with("( add-dir ( 1:c 2:d0 2:d2 ( ) ) ) "));
}

/// A-616 part 2 (`notes/svndiff`'s integers, read back from the writer's own encoding): read
/// integers at their limits and past them; expect each value and length, and none for a cut, an
/// eleventh byte or a value past 64 bits.
#[test]
fn svndiff_integers_are_read_within_the_format() {
  let cases: [(&[u8], Option<(u64, usize)>); 7] = [
    (b"\x00", Some((0, 1))),
    (b"\x7f", Some((127, 1))),
    (b"\x81\x00", Some((128, 2))),
    (b"\x81\xff\xff\xff\xff\xff\xff\xff\xff\x7f", Some((u64::MAX, 10))),
    (b"\x82\xff\xff\xff\xff\xff\xff\xff\xff\x7f", None),
    (b"\x80\x80\x80\x80\x80\x80\x80\x80\x80\x80\x00", None),
    (b"\x80", None),
  ];
  for (bytes, expected) in cases {
    assert_eq!(read_integer(bytes), expected, "{bytes:?}");
  }
}

/// A-616 part 2 (hostile encodings): split encodings whose lengths do not hold; expect each
/// refused as a corrupt window, and a whole one split into its three sections.
#[test]
fn a_window_encoding_whose_lengths_do_not_hold_is_refused() {
  let corrupt = Err(EditorRefused::Window(SvndiffError::CorruptWindowHeader));
  let encodings: [&[u8]; 4] = [
    b"",
    b"\x00\x00\x03\x01",
    b"\x00\x00\x03\x01\x03\x83",
    b"\x00\x00\x03\x01\x03\x83abcd",
  ];
  for encoded in encodings {
    assert_eq!(window_sections(encoded), corrupt, "{encoded:?}");
  }
  let whole: [&[u8]; 3] = [b"\x00\x00\x03\x01\x03", b"\x83", b"abc"];
  assert_eq!(window_sections(b"\x00\x00\x03\x01\x03\x83abc"), Ok(whole));
}
