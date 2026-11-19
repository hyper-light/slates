//! The reporter's recorded sessions, shared by its tests (A-616): stock Subversion 1.15.0's `svn`
//! against `svnserve` over `history.txt`'s repository, every byte each way, recorded by
//! `crates/svn/fixtures/report/make-fixtures.py` (each file's SHA-256 in `SHA256SUMS`), and the
//! helpers that read their items.
//!
//! Every session is included here, in one table, so each file's path is read from this file's
//! place whichever test asks for it.

use super::editor::{WINDOW_HEADER_INTEGERS, read_integer};
use super::{
  Begun, Context, PARSED_ITEM_BYTES, PATH_INFO_BYTES, Progress, Report, Reporter, Request, RequestKind,
};
use crate::handshake::{ClientCapabilities, Principal, Session};
use crate::index::{NodeKind, Revision};
use crate::ra_svn::{Bounds, Parsed, Parser, Value};
use crate::svndiff::{Window, decode_all};

/// One recorded connection: what the client sent and what `svnserve` sent.
#[derive(Clone, Copy)]
pub(super) struct Recorded {
  /// The session's name and connection, its files' stem.
  pub(super) name: &'static str,
  /// The client's bytes.
  pub(super) client: &'static [u8],
  /// The server's bytes.
  pub(super) server: &'static [u8],
}

/// The recorded connection `$name`, its two files included at build time.
macro_rules! recorded {
  ($name:literal) => {
    Recorded {
      name: $name,
      client: include_bytes!(concat!("../../fixtures/report/", $name, ".client.bin")),
      server: include_bytes!(concat!("../../fixtures/report/", $name, ".server.bin")),
    }
  };
}

/// Every recorded connection, in `make-fixtures.py`'s order.
pub(super) const SESSIONS: &[Recorded] = &[
  recorded!("checkout-r1.1"),
  recorded!("update-to-head.1"),
  recorded!("switch-to-branch.1"),
  recorded!("checkout-depth-empty.1"),
  recorded!("update-set-depth-infinity.1"),
  recorded!("checkout-depth-immediates.1"),
  recorded!("update-depth-files.1"),
  recorded!("checkout-head.1"),
  recorded!("update-file-to-r2.1"),
  recorded!("update-mixed-revisions.1"),
  recorded!("checkout-r2.1"),
  recorded!("status-u.1"),
  recorded!("status-u.2"),
  recorded!("diff-r1-r6.1"),
  recorded!("diff-r1-r6.2"),
  recorded!("checkout-for-switch.1"),
  recorded!("switch-subtree.1"),
  recorded!("update-with-link.1"),
  recorded!("checkout-for-not-present.1"),
  recorded!("update-subtree-to-r1.1"),
  recorded!("update-with-not-present.1"),
  recorded!("checkout-for-lock.1"),
  recorded!("lock-a.1"),
  recorded!("update-with-lock.1"),
  recorded!("update-with-broken-lock.1"),
  recorded!("hostile-report-unknown-command.1"),
  recorded!("hostile-report-first-not-root.1"),
  recorded!("hostile-report-missing-path.1"),
  recorded!("hostile-report-bad-depth.1"),
  recorded!("hostile-report-link-elsewhere.1"),
  recorded!("hostile-report-abort.1"),
  recorded!("hostile-update-depth-exclude.1"),
  recorded!("hostile-report-delete-root.1"),
  recorded!("hostile-report-revision-past-head.1"),
  recorded!("hostile-update-target-absent.1"),
  recorded!("hostile-report-malformed-set-path.1"),
  recorded!("hostile-report-early-edit-response.1"),
  recorded!("hostile-report-client-refuses-the-edit.1"),
];

/// The recorded connection named `name`.
pub(super) fn recorded(name: &str) -> Recorded {
  SESSIONS
    .iter()
    .find(|session| session.name == name)
    .copied()
    .unwrap_or_else(|| panic!("no recorded session {name}"))
}

/// The repository's URL every session reached it at.
pub(super) const ROOT_URL: &[u8] = b"svn://127.0.0.1/repo";

/// The repository's identity (`make-fixtures.py` `UUID`).
pub(super) const UUID: &[u8] = b"00000000-0000-4000-8000-0000000a0616";

/// The capabilities a stock 1.15 client announces.
pub(super) fn stock_capabilities() -> ClientCapabilities {
  ClientCapabilities {
    svndiff1: true,
    accepts_svndiff2: true,
    absent_entries: true,
    depth: true,
    mergeinfo: true,
    log_revprops: true,
  }
}

/// An anonymous session of a stock 1.15 client on the fixtures' repository.
pub(super) fn session() -> Session {
  Session {
    principal: Principal::Anonymous,
    capabilities: stock_capabilities(),
    uuid: UUID.to_vec(),
    root_url: ROOT_URL.to_vec(),
  }
}

/// Bounds that admit any item of `bytes`.
pub(super) fn admitting(bytes: &[u8]) -> Bounds {
  Bounds {
    item_bytes: bytes.len() + 1,
    word_bytes: bytes.len(),
    depth: bytes.len(),
    nodes: bytes.len(),
  }
}

/// `bytes` split into its items, each with its whitespace.
pub(super) fn items(bytes: &[u8]) -> Vec<&[u8]> {
  let mut items = Vec::new();
  let mut rest = bytes;
  while !rest.is_empty() {
    let Parsed::Item { consumed, .. } = Parser::new(admitting(rest)).parse(rest).unwrap() else {
      panic!("the bytes end inside an item");
    };
    items.push(&rest[..consumed]);
    rest = &rest[consumed..];
  }
  items
}

/// The value of one whole item.
pub(super) fn value_of(item: &[u8]) -> Value<'_> {
  match Parser::new(admitting(item)).parse(item).unwrap() {
    Parsed::Item { value, .. } => value,
    Parsed::NeedMore { .. } => panic!("not a whole item"),
  }
}

/// A command item's word and parameters, or none for an item that is no command.
pub(super) fn command(item: &[u8]) -> Option<(String, Vec<Value<'_>>)> {
  match value_of(item) {
    Value::List(parts) => match parts.as_slice() {
      [Value::Word(name), Value::List(params)] => Some(((*name).to_owned(), params.clone())),
      _ => None,
    },
    _ => None,
  }
}

/// The youngest revision of `history.txt`'s repository.
pub(super) const YOUNGEST: Revision = 6;

/// The repository path a URL names, for the tests' own bookkeeping of the session's path.
pub(super) fn path_of(url: &[u8]) -> String {
  let rest = &url[ROOT_URL.len()..];
  if rest.is_empty() {
    "/".to_owned()
  } else {
    String::from_utf8(rest.to_vec()).unwrap()
  }
}

/// Where a recorded connection's report begins: the session's path when its report command came
/// (the answer's URL, then each `reparent`'s), the command's index among the client's items, and
/// its request.
pub(super) fn report_command(recorded: Recorded) -> (String, usize, Request) {
  let items = items(recorded.client);
  let Value::List(answer) = value_of(items[0]) else {
    panic!("{}: an answer", recorded.name)
  };
  let Value::String(url) = answer[2] else {
    panic!("{}: an answer's URL", recorded.name)
  };
  let mut session_path = path_of(url);
  for (index, item) in items.iter().enumerate() {
    let Some((name, params)) = command(item) else {
      continue;
    };
    if name == "reparent" {
      let Value::String(url) = params[0] else {
        panic!("a reparent's URL")
      };
      session_path = path_of(url);
    }
    if let Some(kind) = RequestKind::of(&name) {
      return (session_path, index, Request::read(kind, &params).unwrap());
    }
  }
  panic!("{}: no command begins a report", recorded.name)
}

/// The context a recorded connection's report is read in: everything readable, room to spare.
pub(super) fn context<'a>(session: &'a Session, session_path: &'a str, client: &[u8]) -> Context<'a> {
  Context {
    session,
    session_path,
    youngest: YOUNGEST,
    readable: true,
    room: client.len() * PATH_INFO_BYTES,
    item_bytes: client.len() * PARSED_ITEM_BYTES,
  }
}

/// The report a recorded connection finished, read whole as the reporter reads it.
pub(super) fn finished_report(recorded: Recorded) -> Report {
  let (session_path, index, request) = report_command(recorded);
  let session = session();
  let items = items(recorded.client);
  let report_bytes: Vec<u8> = items[index + 1..].concat();
  let context = context(&session, &session_path, recorded.client);
  let mut written = Vec::new();
  let Begun::Reporting(mut reporter) = Reporter::begin(&context, request, &mut written).unwrap()
  else {
    panic!("{}: the command was refused", recorded.name)
  };
  let mut rest = report_bytes.as_slice();
  loop {
    match reporter.receive(rest, &mut written).unwrap() {
      Progress::Took { consumed } => rest = &rest[consumed..],
      Progress::Finished { report, .. } => return *report,
      other => panic!("{}: the report ended {other:?}", recorded.name),
    }
  }
}

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

/// One drive call as a recording holds it, owned; a delta's chunks decoded into windows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Written {
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
pub(super) struct ReadDrive {
  /// The calls.
  pub(super) calls: Vec<Written>,
  /// Per item, whether its bytes may differ from ours.
  pub(super) compressed: Vec<bool>,
}

/// The items of `server` that are the drive: from its first editor command while they are editor
/// commands.
pub(super) fn drive_items(server: &[u8]) -> Vec<&[u8]> {
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
pub(super) fn read_drive(drive: &[&[u8]]) -> ReadDrive {
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
    assert_eq!(
      self.open.last(),
      Some(&text(token)),
      "not the innermost node"
    );
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
    for section in [
      &rest[..instructions],
      &rest[instructions..instructions + new_data],
    ] {
      let (original, used) = read_integer(section).unwrap();
      compressed |= u64::try_from(section.len() - used).unwrap() < original;
    }
    rest = &rest[instructions + new_data..];
  }
  compressed
}
