//! The report's tests (A-616, part 1; §4.17.4 "4. Reads", the reporter).
//!
//! The oracle is stock Subversion: 1.15.0's own sessions, `svn` against `svnserve` over
//! `history.txt`'s repository, recorded by `crates/svn/fixtures/report/make-fixtures.py` (each
//! file's SHA-256 in `SHA256SUMS`). Each stock session's command and report are replayed through
//! the reporter, whose answers must be exactly what `svnserve` wrote before its drive began, and
//! whose finished report must hold what `reporter.c` kept; each hand-made session's refusal must be
//! written byte for byte as `svnserve` wrote it. Beside them: `vparse_tuple`'s reading as tables,
//! Subversion's canonical paths and URL decoding, the first refusal kept, the bounds, calls out of
//! turn, and every cut of every report.

use super::recorded::{
  ROOT_URL, Recorded, YOUNGEST, context, items, recorded, report_command, session, value_of,
};
use super::*;
use crate::handshake::Session;

/// The items the handshake takes from the server's bytes: greeting, auth request, success,
/// repos-info.
const SERVER_HANDSHAKE_ITEMS: usize = 4;

/// The drive's first command, which ends what the reporter writes.
const TARGET_REV: &[u8] = b"( target-rev ";

/// A trivial auth request, `serve.c`'s `trivial_auth_request`.
const TRIVIAL_AUTH: &[u8] = b"( success ( ( ) 0: ) ) ";

/// How a replayed report ended.
#[derive(Debug, PartialEq, Eq)]
enum Ending {
  /// The command was refused at its beginning.
  Refused(ReportFailure),
  /// The report was finished and checked.
  Finished(Box<Report>),
  /// The report was refused at its end.
  Failed(ReportFailure),
  /// The report was abandoned.
  Aborted,
  /// The connection was refused.
  Ended(Refused),
  /// The client's bytes ran out.
  Waiting,
}

/// A replayed report: what the reporter wrote, how it ended, and how many of the report's bytes it
/// took.
struct Replay {
  written: Vec<u8>,
  ending: Ending,
  taken: usize,
}

/// Replays `report_bytes`, the client's bytes after its report command, through a reporter begun
/// with `request` in `context`, delivered in the pieces `cuts` makes.
fn replay(context: &Context<'_>, request: Request, report_bytes: &[u8], cuts: &[usize]) -> Replay {
  let mut written = Vec::new();
  let mut reporter = match Reporter::begin(context, request, &mut written).unwrap() {
    Begun::Reporting(reporter) => reporter,
    Begun::Failed(failure) => {
      return Replay {
        written,
        ending: Ending::Refused(failure),
        taken: 0,
      };
    }
  };
  let mut pieces = pieces(report_bytes, cuts).into_iter();
  let mut held = Vec::new();
  let mut taken = 0;
  loop {
    match reporter.receive(&held, &mut written) {
      Ok(Progress::NeedMore { .. }) => match pieces.next() {
        Some(piece) => held.extend_from_slice(piece),
        None => {
          return Replay {
            written,
            ending: Ending::Waiting,
            taken,
          };
        }
      },
      Ok(Progress::Took { consumed }) => {
        held.drain(..consumed);
        taken += consumed;
      }
      Ok(progress) => return finished(written, progress, taken),
      Err(refused) => {
        return Replay {
          written,
          ending: Ending::Ended(refused),
          taken,
        };
      }
    }
  }
}

/// The replay of a report that ended with `progress`.
fn finished(written: Vec<u8>, progress: Progress, taken: usize) -> Replay {
  let (ending, consumed) = match progress {
    Progress::Finished { report, consumed } => (Ending::Finished(report), consumed),
    Progress::Failed { failure, consumed } => (Ending::Failed(failure), consumed),
    Progress::Aborted { consumed } => (Ending::Aborted, consumed),
    other => panic!("not an end: {other:?}"),
  };
  Replay {
    written,
    ending,
    taken: taken + consumed,
  }
}

/// `bytes` in the pieces `cuts` makes.
fn pieces<'a>(bytes: &'a [u8], cuts: &[usize]) -> Vec<&'a [u8]> {
  let mut pieces = Vec::new();
  let mut start = 0;
  for cut in cuts {
    let end = (*cut).clamp(start, bytes.len());
    pieces.push(&bytes[start..end]);
    start = end;
  }
  pieces.push(&bytes[start..]);
  pieces
}

/// Replays a recorded connection's report whole: its session's path, the replay, and the client's
/// bytes from its report command's end.
fn replay_recorded(recorded: Recorded) -> (String, Replay, Vec<u8>) {
  let (session_path, index, request) = report_command(recorded);
  let session = session();
  let items = items(recorded.client);
  let report_bytes: Vec<u8> = items[index + 1..].concat();
  let context = context(&session, &session_path, recorded.client);
  let replay = replay(&context, request, &report_bytes, &[]);
  (session_path, replay, report_bytes)
}

/// Asserts `needle` occurs in `haystack`, shown as text when it does not.
fn assert_contains(haystack: &[u8], needle: &[u8], what: &str) {
  assert!(
    haystack
      .windows(needle.len())
      .any(|window| window == needle),
    "{what}: {:?} not in {:?}",
    String::from_utf8_lossy(needle),
    String::from_utf8_lossy(haystack)
  );
}

/// A path of a report.
fn path(path: &str, revision: Option<Revision>, depth: Depth, start_empty: bool) -> PathInfo {
  PathInfo {
    path: path.to_owned(),
    link_path: None,
    revision,
    depth,
    start_empty,
    lock_token: None,
  }
}

/// The report of `/trunk`'s working copy updated to `target_revision` from `source_revision`.
fn trunk_update(
  target_revision: Revision,
  source_revision: Revision,
  target_info: PathInfo,
) -> Report {
  Report {
    kind: RequestKind::Update,
    anchor: "/trunk".to_owned(),
    operand: String::new(),
    target_path: "/trunk".to_owned(),
    target_revision,
    source_revision,
    requested_depth: Depth::Unknown,
    text_deltas: true,
    send_copyfrom_args: false,
    ignore_ancestry: false,
    is_switch: false,
    target_info,
    paths: Vec::new(),
  }
}

/// The report the hidden `/trunk/secret` adds once a working copy holds it absent: a deletion.
fn secret() -> PathInfo {
  path("secret", None, Depth::Infinity, false)
}

/// Replays `recorded`'s report and expects `expected` finished, its answers exactly what `svnserve`
/// wrote just before its drive: a trivial auth request for the command and one for `finish-report`.
fn assert_finished(recorded: Recorded, expected: Report) {
  let (_, replay, report_bytes) = replay_recorded(recorded);
  assert_eq!(
    replay.ending,
    Ending::Finished(Box::new(expected)),
    "{}",
    recorded.name
  );
  assert_eq!(
    replay.taken,
    report_bytes.len() - items(&report_bytes).last().unwrap().len()
  );
  assert_eq!(
    replay.written,
    [TRIVIAL_AUTH, TRIVIAL_AUTH].concat(),
    "{}",
    recorded.name
  );
  assert_contains(
    recorded.server,
    &[&replay.written, TARGET_REV].concat(),
    recorded.name,
  );
}

/// A-616 (stock checkouts: `svn checkout -r1`, at depth empty and at depth immediates; protocol
/// §3.1.1 `update`, `reporter.c` `finish_report`): replay each command and its report; expect the
/// trivial auth requests `svnserve` wrote before its drive, and the report it kept: the target at
/// the revision asked, its depth and start-empty as reported, nothing more.
#[test]
fn a_stock_checkouts_report_is_kept_as_svnserve_keeps_it() {
  assert_finished(
    recorded("checkout-r1.1"),
    trunk_update(1, 1, path("", Some(1), Depth::Infinity, true)),
  );
  let mut empty = trunk_update(6, 6, path("", Some(6), Depth::Empty, true));
  empty.requested_depth = Depth::Empty;
  assert_finished(recorded("checkout-depth-empty.1"), empty);
  let mut immediates = trunk_update(6, 6, path("", Some(6), Depth::Immediates, true));
  immediates.requested_depth = Depth::Immediates;
  assert_finished(recorded("checkout-depth-immediates.1"), immediates);
}

/// A-616 (stock updates: to the head over r1, to depth infinity, at depth files, of mixed
/// revisions; `serve.c` `set_path` and `delete_path`): replay each; expect `svnserve`'s answers and
/// its report, the hidden path reported deleted, a file at another revision kept with it, depths as
/// reported.
#[test]
fn a_stock_updates_report_is_kept_as_svnserve_keeps_it() {
  let mut to_head = trunk_update(6, 1, path("", Some(1), Depth::Infinity, false));
  to_head.paths = vec![secret()];
  assert_finished(recorded("update-to-head.1"), to_head);
  let mut deeper = trunk_update(6, 6, path("", Some(6), Depth::Empty, false));
  deeper.requested_depth = Depth::Infinity;
  assert_finished(recorded("update-set-depth-infinity.1"), deeper);
  let mut files = trunk_update(6, 6, path("", Some(6), Depth::Files, false));
  files.paths = vec![secret()];
  assert_finished(recorded("update-depth-files.1"), files);
  let mut mixed = trunk_update(6, 6, path("", Some(6), Depth::Infinity, false));
  mixed.paths = vec![secret(), path("a.txt", Some(2), Depth::Infinity, false)];
  assert_finished(recorded("update-mixed-revisions.1"), mixed);
}

/// A-616 (stock updates of a target beneath the anchor: one file to r2, a subtree to a revision
/// without it; `write_path_info` joins each reported path to the target): replay each; expect the
/// target the operand, its report joined to it.
#[test]
fn a_stock_update_of_a_target_joins_its_report_to_the_target() {
  let mut file = trunk_update(2, 6, path("a.txt", Some(6), Depth::Infinity, false));
  file.operand = "a.txt".to_owned();
  file.target_path = "/trunk/a.txt".to_owned();
  assert_finished(recorded("update-file-to-r2.1"), file);
  let mut subtree = trunk_update(1, 6, path("sub", Some(6), Depth::Infinity, false));
  subtree.anchor = "/trunk/dir".to_owned();
  subtree.operand = "sub".to_owned();
  subtree.target_path = "/trunk/dir/sub".to_owned();
  assert_finished(recorded("update-subtree-to-r1.1"), subtree);
}

/// A-616 (stock reports of a not-present path, a lock and a switched subtree: `delete-path`,
/// `set-path` with a lock token, `link-path` with its URL made a repository path by `get_fs_path`):
/// replay each; expect each kept as `reporter.c` keeps it.
#[test]
fn a_stock_reports_deletions_locks_and_links_are_kept() {
  let mut not_present = trunk_update(6, 6, path("", Some(6), Depth::Infinity, false));
  not_present.paths = vec![path("dir/sub", None, Depth::Infinity, false), secret()];
  assert_finished(recorded("update-with-not-present.1"), not_present);
  let (_, replay, _) = replay_recorded(recorded("update-with-lock.1"));
  let Ending::Finished(report) = replay.ending else {
    panic!("{:?}", replay.ending)
  };
  let reported = report
    .paths
    .iter()
    .find(|info| info.path == "a.txt")
    .unwrap();
  let token = reported.lock_token.clone().unwrap();
  assert!(token.starts_with(b"opaquelocktoken:"), "{token:?}");
  let mut locked = trunk_update(6, 6, path("", Some(6), Depth::Infinity, false));
  let mut held = path("a.txt", Some(6), Depth::Infinity, false);
  held.lock_token = Some(token);
  locked.paths = vec![secret(), held];
  assert_finished(recorded("update-with-lock.1"), locked);
  let mut linked = trunk_update(6, 6, path("", Some(6), Depth::Infinity, false));
  let mut switched = path("dir", Some(6), Depth::Infinity, false);
  switched.link_path = Some("/branches/b1/dir".to_owned());
  linked.paths = vec![switched, secret()];
  assert_finished(recorded("update-with-link.1"), linked);
}

/// A-616 (stock `switch` of the whole and of a subtree, `status -u` and a URL `diff`: `serve.c`
/// `switch_cmd`, `status`, `diff`): replay each; expect the target path the URL's repository path,
/// the ancestry and texts as each command sets them, and `diff`'s anchor the repository's root.
#[test]
fn a_stock_switch_status_and_diff_report_where_their_commands_say() {
  let mut switch = trunk_update(6, 6, path("", Some(6), Depth::Infinity, false));
  switch.kind = RequestKind::Switch;
  switch.target_path = "/branches/b1".to_owned();
  switch.is_switch = true;
  switch.paths = vec![secret()];
  assert_finished(recorded("switch-to-branch.1"), switch);
  let mut subtree = trunk_update(6, 6, path("dir", Some(6), Depth::Infinity, false));
  subtree.kind = RequestKind::Switch;
  subtree.operand = "dir".to_owned();
  subtree.target_path = "/branches/b1/dir".to_owned();
  subtree.is_switch = true;
  assert_finished(recorded("switch-subtree.1"), subtree);
  let mut status = trunk_update(6, 2, path("", Some(2), Depth::Infinity, false));
  status.kind = RequestKind::Status;
  status.text_deltas = false;
  status.paths = vec![secret()];
  assert_finished(recorded("status-u.1"), status);
  let mut diff = trunk_update(6, 1, path("trunk", Some(1), Depth::Infinity, false));
  diff.kind = RequestKind::Diff;
  diff.anchor = "/".to_owned();
  diff.operand = "trunk".to_owned();
  diff.is_switch = true;
  diff.ignore_ancestry = true;
  assert_finished(recorded("diff-r1-r6.1"), diff);
}

/// The server's items after the handshake, joined.
fn after_handshake(server: &[u8]) -> Vec<u8> {
  items(server)[SERVER_HANDSHAKE_ITEMS..].concat()
}

/// The server's items after the handshake up to the drive's first command, joined.
fn before_drive(server: &[u8]) -> Vec<u8> {
  let all = after_handshake(server);
  let end = all
    .windows(TARGET_REV.len())
    .position(|window| window == TARGET_REV)
    .unwrap_or(all.len());
  all[..end].to_vec()
}

/// Replays a hand-made session's report and returns its replay.
fn replay_hostile(recorded: Recorded) -> Replay {
  replay_recorded(recorded).1
}

/// A-616 (hand-made reports `svnserve` refuses at `finish-report`: a first path that is no
/// `set-path` of the target, a depth it does not know, a link URL of another repository, a deletion
/// of the anchor beside its report): replay each; expect every byte `svnserve` wrote after the
/// handshake, and each failure typed with its number.
#[test]
fn a_report_svnserve_refuses_at_its_end_is_refused_in_its_words() {
  let cases = [
    (
      recorded("hostile-report-first-not-root.1"),
      ReportFailure::InvalidTopLevel,
      165_004,
    ),
    (
      recorded("hostile-report-bad-depth.1"),
      ReportFailure::UnsupportedDepth,
      165_002,
    ),
    (
      recorded("hostile-report-delete-root.1"),
      ReportFailure::TwoTopLevelReports,
      165_004,
    ),
  ];
  for (recorded, failure, code) in cases {
    let replay = replay_hostile(recorded);
    assert_eq!(
      replay.ending,
      Ending::Failed(failure.clone()),
      "{}",
      recorded.name
    );
    assert_eq!(
      replay.written,
      after_handshake(recorded.server),
      "{}",
      recorded.name
    );
    assert_eq!(failure.subversion_code(), code);
  }
  let elsewhere = recorded("hostile-report-link-elsewhere.1");
  let replay = replay_hostile(elsewhere);
  let failure = ReportFailure::NotSameRepository {
    url: "svn://example.org/other/dir".to_owned(),
    repository: "svn://127.0.0.1/repo".to_owned(),
  };
  assert_eq!(replay.ending, Ending::Failed(failure.clone()));
  assert_eq!(replay.written, after_handshake(elsewhere.server));
  assert_eq!(failure.subversion_code(), 170_000);
}

/// A-616 (hand-made sessions answered without a drive: `abort-report`, answered `( success ( ) )`,
/// and a requested depth of `exclude`, refused by `svn_repos_begin_report3` before any report is
/// read): replay each; expect `svnserve`'s bytes, those the connection's command loop then answers
/// aside.
#[test]
fn an_abandoned_report_and_an_excluded_request_are_answered_as_svnserve_answers_them() {
  let aborted = recorded("hostile-report-abort.1");
  let replay = replay_hostile(aborted);
  assert_eq!(replay.ending, Ending::Aborted);
  assert_eq!(replay.written, after_handshake(aborted.server));
  let excluded = recorded("hostile-update-depth-exclude.1");
  let replay = replay_hostile(excluded);
  assert_eq!(
    replay.ending,
    Ending::Refused(ReportFailure::RequestDepthExclude)
  );
  let first_two =
    items(excluded.server)[SERVER_HANDSHAKE_ITEMS..SERVER_HANDSHAKE_ITEMS + 2].concat();
  assert_eq!(replay.written, first_two);
}

/// A-616 (hand-made reports that reach the drive: an unknown command amid the report, answered
/// 210001 at once while the report goes on; a missing path, a source revision past the head, an
/// absent target, an early answer and a client refusing the edit, each finished whole): replay
/// each; expect what `svnserve` wrote before its drive, or before its abort.
#[test]
fn a_report_that_reaches_the_drive_is_answered_as_svnserve_answered_it_before() {
  for recorded in [
    recorded("hostile-report-unknown-command.1"),
    recorded("hostile-report-missing-path.1"),
    recorded("hostile-update-target-absent.1"),
    recorded("hostile-report-client-refuses-the-edit.1"),
  ] {
    let replay = replay_hostile(recorded);
    assert!(
      matches!(replay.ending, Ending::Finished(_)),
      "{}",
      recorded.name
    );
    assert_eq!(
      replay.written,
      before_drive(recorded.server),
      "{}",
      recorded.name
    );
  }
  for recorded in [
    recorded("hostile-report-revision-past-head.1"),
    recorded("hostile-report-early-edit-response.1"),
  ] {
    let replay = replay_hostile(recorded);
    assert!(
      matches!(replay.ending, Ending::Finished(_)),
      "{}",
      recorded.name
    );
    assert_contains(
      recorded.server,
      &[&replay.written, b"( abort-edit ".as_slice()].concat(),
      recorded.name,
    );
  }
}

/// A-616 (a report command `vparse_tuple` refuses ends the connection:
/// `( set-path ( 0: six false ) )`, a word where its revision stands; `svnserve` ended the recorded
/// connection before it flushed the trivial auth request it had answered `update` with): replay it;
/// expect the connection refused, "Malformed network data", 210004.
#[test]
fn a_malformed_report_command_ends_the_connection() {
  let replay = replay_hostile(recorded("hostile-report-malformed-set-path.1"));
  assert_eq!(replay.ending, Ending::Ended(Refused::Malformed));
  assert_eq!(Refused::Malformed.subversion_code(), Some(210_004));
}

/// One `vparse_tuple` case: the items, the format, and the fields or the refusal.
type TupleCase = (
  &'static [u8],
  &'static [u8],
  Result<Vec<Field<'static>>, tuple::Malformed>,
);

/// The items of a written list, `( ... )`, as a slice of values.
fn list_items(written: &'static [u8]) -> Vec<Value<'static>> {
  match value_of(written) {
    Value::List(items) => items,
    other => panic!("not a list: {other:?}"),
  }
}

/// A-616 (`marshal.c` `vparse_tuple`, ported: items read beside the format; past it ignored; a `?`
/// where items stop leaving the rest absent, booleans false, revisions none; a stop elsewhere, or
/// an item of another kind, refused; a number past 2^63 - 1 no revision): read each case; expect
/// its fields or its refusal.
#[test]
fn a_tuple_is_read_as_vparse_tuple_reads_it() {
  let cases: [TupleCase; 10] = [
    (
      b"( ( 6 ) 0: true unknown false false ) ",
      UPDATE_FORMAT,
      Ok(vec![
        Field::Revision(Some(6)),
        Field::String(Some(b"")),
        Field::Boolean(true),
        Field::Word(Some("unknown")),
        Field::Tristate(Some(false)),
        Field::Tristate(Some(false)),
      ]),
    ),
    (
      b"( ( ) 0: true ) ",
      UPDATE_FORMAT,
      Ok(vec![
        Field::Revision(None),
        Field::String(Some(b"")),
        Field::Boolean(true),
        Field::Word(None),
        Field::Tristate(None),
        Field::Tristate(None),
      ]),
    ),
    (
      b"( ( ) 0: true infinity ) ",
      UPDATE_FORMAT,
      Err(tuple::Malformed),
    ),
    (b"( ( 6 ) 0: maybe ) ", UPDATE_FORMAT, Err(tuple::Malformed)),
    (
      b"( ( 18446744073709551615 ) 0: true ) ",
      UPDATE_FORMAT,
      Ok(vec![
        Field::Revision(None),
        Field::String(Some(b"")),
        Field::Boolean(true),
        Field::Word(None),
        Field::Tristate(None),
        Field::Tristate(None),
      ]),
    ),
    (
      b"( 0: 6 false ) ",
      SET_PATH_FORMAT,
      Ok(vec![
        Field::String(Some(b"")),
        Field::Revision(Some(6)),
        Field::Boolean(false),
        Field::String(None),
        Field::Word(None),
      ]),
    ),
    (
      b"( 1:a 6 false ( 3:tok ) files extra 7 ) ",
      SET_PATH_FORMAT,
      Ok(vec![
        Field::String(Some(b"a")),
        Field::Revision(Some(6)),
        Field::Boolean(false),
        Field::String(Some(b"tok")),
        Field::Word(Some("files")),
      ]),
    ),
    (
      b"( 1:a 6 false ( 7 ) ) ",
      SET_PATH_FORMAT,
      Err(tuple::Malformed),
    ),
    (b"( 1:a 6 ) ", SET_PATH_FORMAT, Err(tuple::Malformed)),
    (
      b"( 1:a ) ",
      DELETE_PATH_FORMAT,
      Ok(vec![Field::String(Some(b"a"))]),
    ),
  ];
  for (written, format, expected) in cases {
    let items = list_items(written);
    let read = tuple::read(&items, format);
    assert_eq!(read, expected, "{:?}", String::from_utf8_lossy(written));
  }
}

/// A-616 (`dirent_uri.c` `canonicalize` for a relative path: empty and `.` names dropped, `..`
/// kept; `svn_relpath_join` and `svn_fspath__join`): canonicalize and join each case; expect
/// Subversion's.
#[test]
fn paths_take_subversions_canonical_forms() {
  for (path, canonical) in [
    ("", ""),
    ("/", ""),
    ("a/b", "a/b"),
    ("a//b/", "a/b"),
    ("./a/./b/.", "a/b"),
    ("/a/../b", "a/../b"),
  ] {
    assert_eq!(canonical_relpath(path), canonical, "{path:?}");
  }
  assert_eq!(canonical_fspath("trunk/"), "/trunk");
  assert_eq!(join_relpath("", "a"), "a");
  assert_eq!(join_relpath("a", ""), "a");
  assert_eq!(join_relpath("a", "b/c"), "a/b/c");
  assert_eq!(join_fspath("/", "trunk"), "/trunk");
  assert_eq!(join_fspath("/trunk", ""), "/trunk");
  assert_eq!(join_fspath("/trunk", "dir"), "/trunk/dir");
}

/// A-616 (`path.c` `svn_path_uri_decode`: `%` and two hexadecimal digits of either case the byte
/// they name, a `%` without them kept, `+` a space past a `?` alone; `serve.c` `get_fs_path`):
/// decode each case; expect Subversion's; and the repository path of URLs inside, at and beside the
/// repository.
#[test]
fn urls_decode_as_subversion_decodes_them_and_name_paths_in_the_repository() {
  for (url, decoded) in [
    ("svn://h/r/a%20b", b"svn://h/r/a b".to_vec()),
    ("svn://h/r/%7e%7E", b"svn://h/r/~~".to_vec()),
    ("svn://h/r/50%", b"svn://h/r/50%".to_vec()),
    ("svn://h/r/%zz", b"svn://h/r/%zz".to_vec()),
    ("svn://h/r/a+b?c+d", b"svn://h/r/a+b?c d".to_vec()),
    ("svn://h/r/%ff", b"svn://h/r/\xff".to_vec()),
  ] {
    assert_eq!(uri_decode(url), decoded, "{url}");
  }
  assert_eq!(
    repository_path(ROOT_URL, "svn://127.0.0.1/repo"),
    Ok("/".to_owned())
  );
  assert_eq!(
    repository_path(ROOT_URL, "svn://127.0.0.1/repo/"),
    Ok("/".to_owned())
  );
  assert_eq!(
    repository_path(ROOT_URL, "svn://127.0.0.1/repo/branches/b1%2Fx"),
    Ok("/branches/b1/x".to_owned())
  );
  for beside in [
    "svn://127.0.0.1/repo2",
    "svn://127.0.0.1/rep",
    "svn://other/repo",
    "svn://127.0.0.1/repo/%ff",
  ] {
    assert!(
      matches!(
        repository_path(ROOT_URL, beside),
        Err(ReportFailure::NotSameRepository { .. })
      ),
      "{beside}"
    );
  }
}

/// A string item, `length:bytes `, its length counted.
fn string(bytes: &[u8]) -> Vec<u8> {
  [format!("{}:", bytes.len()).as_bytes(), bytes, b" "].concat()
}

/// The context of a hand-made report on `/trunk` with the given room.
fn trunk_context(session: &Session, room: usize) -> Context<'_> {
  Context {
    session,
    session_path: "/trunk",
    youngest: YOUNGEST,
    readable: true,
    room,
    item_bytes: 1 << 16,
  }
}

/// The request of a hand-made `update` of `/trunk` to the head.
fn update_head() -> Request {
  Request {
    kind: RequestKind::Update,
    revision: Some(YOUNGEST),
    target: String::new(),
    depth: Depth::Infinity,
    url: None,
    text_deltas: true,
    send_copyfrom_args: false,
    ignore_ancestry: false,
  }
}

/// A-616 (`serve.c`: a report's first refusal kept and the rest read and dropped; `if (!b->err)`
/// before each recording): report a depth Subversion does not know, then a link elsewhere, then a
/// good path; expect the first refusal answered at the end, alone.
#[test]
fn a_reports_first_refusal_is_the_one_answered() {
  let session = session();
  let report = [
    b"( set-path ( 0: 6 false ( ) infinity ) ) ( set-path ( 1:a 6 false ( ) sideways ) ) "
      .as_slice(),
    b"( link-path ( 1:b ",
    &string(b"svn://elsewhere/r/b"),
    b"6 false ( ) infinity ) ) ( set-path ( 1:c 6 false ) ) ( finish-report ( ) ) ",
  ]
  .concat();
  let replay = replay(
    &trunk_context(&session, 1 << 16),
    update_head(),
    &report,
    &[],
  );
  assert_eq!(
    replay.ending,
    Ending::Failed(ReportFailure::UnsupportedDepth)
  );
  let failure = [
    b"( failure ( ( 165002 ".as_slice(),
    &string(b"Unsupported report depth 'unknown'"),
    b"0: 0 ) ) ) ",
  ]
  .concat();
  assert_eq!(
    replay.written,
    [TRIVIAL_AUTH, TRIVIAL_AUTH, &failure].concat()
  );
}

/// A-616 (a link at depth `exclude`, `svn_repos_link_path3`; a second report of a target beneath
/// the anchor taken as the target's, a deletion there keeping the first report's depth,
/// `finish_report`): report each; expect the link refused 165002 and the deletion taken with the
/// first depth.
#[test]
fn a_link_at_depth_exclude_is_refused_and_a_deleted_target_keeps_its_depth() {
  let session = session();
  let report = [
    b"( set-path ( 0: 6 false ( ) infinity ) ) ( link-path ( 1:b ".as_slice(),
    &string(b"svn://127.0.0.1/repo/b/x"),
    b"6 false ( ) exclude ) ) ( finish-report ( ) ) ",
  ]
  .concat();
  let excluded = replay(
    &trunk_context(&session, 1 << 16),
    update_head(),
    &report,
    &[],
  );
  assert_eq!(excluded.ending, Ending::Failed(ReportFailure::ExcludeLink));
  let mut request = update_head();
  request.target = "a.txt".to_owned();
  let report =
    b"( set-path ( 0: 6 false ( ) files ) ) ( delete-path ( 0: ) ) ( finish-report ( ) ) ";
  let deleted = replay(&trunk_context(&session, 1 << 16), request, report, &[]);
  let Ending::Finished(finished) = deleted.ending else {
    panic!("{:?}", deleted.ending)
  };
  assert_eq!(
    finished.target_info,
    path("a.txt", None, Depth::Files, false)
  );
  assert_eq!(finished.source_revision, 6);
}

/// A-616 (`update` of a target the principal may not read, `must_have_access`; a `switch` URL of
/// another repository, `get_fs_path` before any report): begin each; expect "Authorization failed"
/// 170001 with no auth request before it, and the switch refused 170000 after its trivial one.
#[test]
fn an_unreadable_update_and_a_foreign_switch_are_refused_before_their_report() {
  let session = session();
  let mut context = trunk_context(&session, 1 << 16);
  context.readable = false;
  let mut written = Vec::new();
  let begun = Reporter::begin(&context, update_head(), &mut written).unwrap();
  assert!(matches!(begun, Begun::Failed(ReportFailure::NotAuthorized)));
  assert_eq!(
    written,
    b"( failure ( ( 170001 20:Authorization failed 0: 0 ) ) ) "
  );
  let mut switch = update_head();
  switch.kind = RequestKind::Switch;
  switch.url = Some("svn://elsewhere/repo/branches".to_owned());
  let mut written = Vec::new();
  let begun = Reporter::begin(&trunk_context(&session, 1 << 16), switch, &mut written).unwrap();
  assert!(matches!(
    begun,
    Begun::Failed(ReportFailure::NotSameRepository { .. })
  ));
  assert!(written.starts_with(TRIVIAL_AUTH));
}

/// A-616 (the report's room, the caller's bytes: past it the connection is refused 210009; a report
/// command's lists at most three deep): report paths until the room is spent; expect the path that
/// passes it refused, and one byte more of room taking it; and a set-path nested a level deeper
/// refused.
#[test]
fn a_report_past_its_room_or_nested_too_deep_ends_the_connection() {
  let session = session();
  let report =
    b"( set-path ( 0: 6 false ( ) infinity ) ) ( set-path ( 1:a 6 false ( ) infinity ) ) \
( finish-report ( ) ) ";
  let needed = 2 * PATH_INFO_BYTES + 1;
  let tight = replay(&trunk_context(&session, needed), update_head(), report, &[]);
  assert!(matches!(tight.ending, Ending::Finished(_)));
  let short = replay(
    &trunk_context(&session, needed - 1),
    update_head(),
    report,
    &[],
  );
  assert_eq!(short.ending, Ending::Ended(Refused::ReportTooLarge));
  assert_eq!(Refused::ReportTooLarge.subversion_code(), Some(210_009));
  let deep = b"( set-path ( 0: 6 false ( ( 3:tok ) ) infinity ) ) ";
  let replay = replay(&trunk_context(&session, 1 << 16), update_head(), deep, &[]);
  assert!(matches!(
    replay.ending,
    Ending::Ended(Refused::Item(ItemError::NestedTooDeep { .. }))
  ));
}

/// A-616 (hostile bytes in a report: a path or token holding a NUL byte, which `svnserve` cuts at
/// the NUL, and a path that is no UTF-8; stricter than `svnserve`): report each; expect the
/// connection refused, typed.
#[test]
fn a_report_path_or_token_holding_a_nul_or_no_utf8_is_refused() {
  let session = session();
  for report in [
    b"( set-path ( 3:a\x00b 6 false ( ) infinity ) ) ".as_slice(),
    b"( set-path ( 0: 6 false ( 3:t\x00k ) infinity ) ) ",
    b"( delete-path ( 2:\xff\xfe ) ) ",
  ] {
    let replay = replay(
      &trunk_context(&session, 1 << 16),
      update_head(),
      report,
      &[],
    );
    assert_eq!(
      replay.ending,
      Ending::Ended(Refused::Malformed),
      "{report:?}"
    );
  }
}

/// A-616 (calls out of turn are the caller's error): finish a report, then receive again; expect
/// `OutOfTurn`, nothing written, and no Subversion number.
#[test]
fn a_call_after_the_report_is_out_of_turn() {
  let session = session();
  let mut written = Vec::new();
  let Begun::Reporting(mut reporter) = Reporter::begin(
    &trunk_context(&session, 1 << 16),
    update_head(),
    &mut written,
  )
  .unwrap() else {
    panic!("an update begins a report");
  };
  let report = b"( set-path ( 0: 6 false ( ) infinity ) ) ";
  assert_eq!(
    reporter.receive(report, &mut written),
    Ok(Progress::Took {
      consumed: report.len()
    })
  );
  let finish = b"( finish-report ( ) ) ";
  assert!(matches!(
    reporter.receive(finish, &mut written),
    Ok(Progress::Finished { .. })
  ));
  let before = written.clone();
  assert_eq!(
    reporter.receive(finish, &mut written),
    Err(Refused::OutOfTurn)
  );
  assert_eq!(written, before);
  assert_eq!(Refused::OutOfTurn.subversion_code(), None);
}

/// A-616 (resumable at any cut, the item codec's promise carried through the report): replay every
/// recorded report in two pieces at every cut; expect the whole's answers and ending.
#[test]
fn every_recorded_report_cut_anywhere_is_read_as_the_whole() {
  for recorded in [
    recorded("update-to-head.1"),
    recorded("update-with-lock.1"),
    recorded("update-with-link.1"),
    recorded("hostile-report-unknown-command.1"),
    recorded("hostile-report-link-elsewhere.1"),
  ] {
    let (session_path, index, request) = report_command(recorded);
    let session = session();
    let report_bytes: Vec<u8> = items(recorded.client)[index + 1..].concat();
    let context = context(&session, &session_path, recorded.client);
    let whole = replay(&context, request.clone(), &report_bytes, &[]);
    for cut in 1..report_bytes.len() {
      let split = replay(&context, request.clone(), &report_bytes, &[cut]);
      assert_eq!(split.written, whole.written, "{} at {cut}", recorded.name);
      assert_eq!(split.ending, whole.ending, "{} at {cut}", recorded.name);
    }
  }
}
