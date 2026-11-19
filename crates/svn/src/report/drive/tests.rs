//! The drive's tests (A-616, part 2; §4.17.4 "4. Reads", the reporter).
//!
//! The oracle is stock `svnserve` 1.15.0: every drive it made in the recorded sessions
//! (`crates/svn/fixtures/report/`, each file's SHA-256 in `SHA256SUMS`) against the repository
//! `history.txt` describes. The tests build that repository into an index and a green
//! ([`super::green`]), check it against `svnlook changed`, replay each session's own report through
//! [`Drive`], and compare its calls with `svnserve`'s: call for call, a node's own properties and a
//! directory's deletions as sets (APR's hash order is `svnserve`'s, name order slates'), and every
//! text by what its delta makes of the base its MD5 names. Beside them: FSFS's closest copy and the
//! revision a path was deleted in, copy sources, pausing at any step, and generated hostile
//! reports, whose drives must end and stay in the editor's order.

use super::green::{CHANGED, FixtureGuards, Green, md5, repository};
use super::*;
use crate::handshake::ClientCapabilities;
use crate::ra_svn::Value;
use crate::report::RequestKind;
use crate::report::editor::EditorWriter;
use crate::report::recorded::{
  SESSIONS, Written, YOUNGEST, command, drive_items, finished_report, items, read_drive,
};
use crate::svndiff::apply_all;
use proptest::prelude::*;

/// The steps a test gives each call: past any one call's walk in the fixtures.
const STEPS: usize = 1 << 20;

/// The drives the replay test must compare: the 22 stock sessions' and five hand-made ones' (the
/// early answer's is the handler's).
const COMPARED_DRIVES: usize = 27;

/// The prefix that begins every entry property's name.
const ENTRY_PREFIX: &str = "svn:entry:";

/// The most nodes a writer checking a generated drive holds open: past any path a report names.
const MOST_OPEN: usize = 64;

/// A drive's calls, owned as a recording's are.
fn run(drive: &mut Drive, index: &Index, green: &Green, guards: &FixtureGuards) -> Vec<Written> {
  let mut calls = Vec::new();
  let mut delta: Option<(Option<[u8; 16]>, Vec<Window>)> = None;
  loop {
    let call = match drive.next(index, green, guards, STEPS) {
      Next::Call(call) => call,
      Next::Paused => panic!("a call needed more steps than {STEPS}"),
      Next::Ended => return calls,
    };
    match call {
      Call::ApplyTextDelta { base_md5 } => delta = Some((base_md5, Vec::new())),
      Call::Window(window) => delta.as_mut().unwrap().1.push(window.clone()),
      Call::TextDeltaEnd => {
        let (base, windows) = delta.take().unwrap();
        calls.push(Written::Delta(base, 0, windows));
      }
      other => calls.push(owned(other)),
    }
  }
}

/// A call that is no part of a delta, owned.
fn owned(call: Call<'_>) -> Written {
  match call {
    Call::TargetRevision(revision) => Written::TargetRevision(revision),
    Call::OpenRoot { base, .. } => Written::OpenRoot(base),
    Call::Open {
      kind, path, base, ..
    } => Written::Open(kind, path.to_owned(), base),
    Call::Add {
      kind,
      path,
      copy_from,
      ..
    } => Written::Add(
      kind,
      path.to_owned(),
      copy_from.map(|(source, revision)| (source.to_owned(), revision)),
    ),
    Call::Delete { path, revision } => Written::Delete(path.to_owned(), revision),
    Call::Absent { kind, path } => Written::Absent(kind, path.to_owned()),
    Call::ChangeProperty { name, value } => {
      Written::ChangeProperty(name.to_owned(), value.map(<[u8]>::to_vec))
    }
    Call::CloseFile { md5 } => Written::CloseFile(md5),
    Call::CloseDirectory => Written::CloseDirectory,
    Call::CloseEdit => Written::CloseEdit,
    Call::AbortEdit => Written::AbortEdit,
    delta => panic!("{delta:?} belongs to a delta"),
  }
}

/// A drive's calls in a form two drives of one edit share.
#[derive(Debug, PartialEq, Eq)]
enum Normal {
  /// A call as it was.
  Call(Written),
  /// A run of property changes on one node: the entry properties in their order, then the rest
  /// by name.
  Properties(Vec<(String, Option<Vec<u8>>)>),
  /// A run of deletions, by path.
  Deletions(Vec<(String, Option<u64>)>),
  /// A delta: the base's MD5, and the MD5 and length of what it makes of the base.
  Delta(Option<[u8; 16]>, [u8; 16], usize),
}

/// The calls in their shared form, every delta applied to the text whose MD5 it names.
fn normalize(calls: Vec<Written>, texts: &BTreeMap<[u8; 16], Vec<u8>>) -> Vec<Normal> {
  let mut normal = Vec::new();
  for call in calls {
    match call {
      Written::ChangeProperty(name, value) => match normal.last_mut() {
        Some(Normal::Properties(run)) => run.push((name, value)),
        _ => normal.push(Normal::Properties(vec![(name, value)])),
      },
      Written::Delete(path, revision) => match normal.last_mut() {
        Some(Normal::Deletions(run)) => run.push((path, revision)),
        _ => normal.push(Normal::Deletions(vec![(path, revision)])),
      },
      Written::Delta(base, _, windows) => normal.push(applied(base, &windows, texts)),
      call => normal.push(Normal::Call(call)),
    }
  }
  for run in &mut normal {
    match run {
      Normal::Properties(properties) => sort_own_properties(properties),
      Normal::Deletions(deletions) => deletions.sort(),
      _ => {}
    }
  }
  normal
}

/// A run of property changes with its entry properties kept in order and the rest sorted by name.
fn sort_own_properties(properties: &mut [(String, Option<Vec<u8>>)]) {
  let entry = properties
    .iter()
    .take_while(|(name, _)| name.starts_with(ENTRY_PREFIX))
    .count();
  properties[entry..].sort();
}

/// A delta applied to the text whose MD5 it names, none naming no bytes.
fn applied(
  base: Option<[u8; 16]>,
  windows: &[Window],
  texts: &BTreeMap<[u8; 16], Vec<u8>>,
) -> Normal {
  let source = base.map_or_else(Vec::new, |md5| texts[&md5].clone());
  let made = apply_all(&source, windows).unwrap();
  Normal::Delta(base, md5(&made), made.len())
}

/// The fixtures' texts by MD5, the empty one among them.
fn texts_by_md5(green: &Green) -> BTreeMap<[u8; 16], Vec<u8>> {
  let mut texts: BTreeMap<[u8; 16], Vec<u8>> = green
    .texts
    .values()
    .map(|text| (md5(text), text.clone()))
    .collect();
  texts.insert(md5(b""), Vec::new());
  texts
}

/// The guards a recorded session's drive ran under: the anonymous principal's, and in the session
/// that holds `a.txt`'s lock, the lock its report names.
fn guards_of(name: &str, report: &Report) -> FixtureGuards {
  let mut guards = FixtureGuards::anonymous();
  if name == "update-with-lock.1" {
    let held = report
      .paths
      .iter()
      .find_map(|info| info.lock_token.clone())
      .unwrap();
    guards.locks.insert(String::from("/trunk/a.txt"), held);
  }
  guards
}

/// The refusal `svnserve` wrote after a drive's `abort-edit`: its number and message.
fn recorded_failure(server: &[u8]) -> DriveFailure {
  let items = items(server);
  let aborted = items
    .iter()
    .position(|item| command(item).is_some_and(|(name, _)| name == "abort-edit"))
    .unwrap();
  let (word, params) = command(items[aborted + 1]).unwrap();
  assert_eq!(word, "failure");
  let Value::List(error) = &params[0] else {
    panic!("an error")
  };
  let (Value::Number(code), Value::String(message)) = (&error[0], &error[1]) else {
    panic!("an error's number and message")
  };
  DriveFailure {
    code: u32::try_from(*code).unwrap(),
    message: String::from_utf8(message.to_vec()).unwrap(),
  }
}

/// The drive `svnserve` made in a session, as far as the drive makes it: a refused edit's
/// `abort-edit` after `close-edit` is the handler's, not the drive's.
fn recorded_drive(server: &[u8]) -> Vec<Written> {
  let mut calls = read_drive(&drive_items(server)).calls;
  if let [.., Written::CloseEdit, Written::AbortEdit] = calls.as_slice() {
    calls.pop();
  }
  calls
}

/// A-616 part 2 (the fixtures' repository): render every revision of the index built from
/// `history.txt` as `svnlook changed --copy-info` writes it; expect `changed.txt`, the recorder's
/// own record of the repository `svnserve` served.
#[test]
fn the_history_rebuilt_is_the_one_svnserve_served() {
  let (index, _) = repository();
  let mut rendered = String::new();
  for revision in 1..=YOUNGEST {
    rendered.push_str(&format!("r{revision}\n"));
    for change in index.changes(revision).unwrap() {
      rendered.push_str(&svnlook_line(change));
    }
  }
  let ours: Vec<&str> = rendered.lines().collect();
  assert_eq!(ours, CHANGED.lines().collect::<Vec<_>>());
}

/// One changed path as `svnlook changed --copy-info` writes it.
fn svnlook_line(change: &crate::index::ChangedPath) -> String {
  use crate::index::Action;
  let slash = |kind: NodeKind| if kind == NodeKind::Directory { "/" } else { "" };
  let first = match change.action {
    Action::Add => 'A',
    Action::Delete => 'D',
    Action::Replace => 'R',
    Action::Modify if change.text.is_some() => 'U',
    Action::Modify => '_',
  };
  let second = if change.action == Action::Modify && change.properties.is_some() {
    'U'
  } else {
    ' '
  };
  let copied = if change.copy_from.is_some() { '+' } else { ' ' };
  let path = change.path.get(1..).unwrap();
  let mut line = format!("{first}{second}{copied} {path}{}\n", slash(change.kind));
  if let Some(source) = &change.copy_from {
    let source_path = source.path.get(1..).unwrap();
    line.push_str(&format!(
      "    (from {source_path}{}:r{})\n",
      slash(change.kind),
      source.revision
    ));
  }
  line
}

/// A-616 part 2 (`reporter.c` against stock `svnserve` 1.15.0): replay every recorded session's own
/// report through a drive over the fixtures' repository; expect `svnserve`'s drive call for call (a
/// node's own properties and a directory's deletions as sets, each delta by what it makes of its
/// base), and where `svnserve` aborted, its refusal; every comparable drive met.
#[test]
fn every_recorded_drive_is_the_drive_slates_makes() {
  let (index, green) = repository();
  let texts = texts_by_md5(&green);
  let mut compared = 0;
  for session in SESSIONS {
    let theirs = recorded_drive(session.server);
    if theirs.is_empty() || session.name == "hostile-report-early-edit-response.1" {
      continue;
    }
    compared += 1;
    let aborted = matches!(theirs.last(), Some(Written::AbortEdit));
    let expected = aborted.then(|| recorded_failure(session.server));
    let report = finished_report(*session);
    let guards = guards_of(session.name, &report);
    let mut drive = Drive::new(report, b"00000000-0000-4000-8000-0000000a0616");
    let ours = run(&mut drive, &index, &green, &guards);
    assert_eq!(
      normalize(ours, &texts),
      normalize(theirs, &texts),
      "{}",
      session.name
    );
    assert_eq!(drive.failure().cloned(), expected, "{}", session.name);
  }
  assert_eq!(compared, COMPARED_DRIVES);
}

/// A-616 part 2 (`svn_repos_deleted_rev` and `fs_closest_copy` over the fixtures' history): ask for
/// the revision of each deletion and move, and for the copy over a branch's file and a moved file;
/// expect the revisions `changed.txt` records, and none where nothing was deleted or copied.
#[test]
fn deletions_and_copies_are_found_where_the_history_made_them() {
  let (index, _) = repository();
  let deleted = |path, start, end| history::deleted_revision(&index, path, start, end).unwrap();
  assert_eq!(deleted("/trunk/dir/c.bin", 1, 6), Some(4));
  assert_eq!(deleted("/trunk/new.txt", 2, 6), Some(4));
  assert_eq!(deleted("/trunk/new.txt", 6, 2), Some(4));
  assert_eq!(deleted("/trunk/a.txt", 1, 6), None);
  assert_eq!(deleted("/trunk/new.txt", 1, 6), None);
  let copy = |path, revision| history::closest_copy(&index, path, revision).unwrap();
  assert_eq!(
    copy("/branches/b1/a.txt", 3),
    Some((3, String::from("/branches/b1")))
  );
  assert_eq!(
    copy("/trunk/renamed.txt", 6),
    Some((4, String::from("/trunk/renamed.txt")))
  );
  assert_eq!(copy("/trunk/a.txt", 6), None);
  assert_eq!(copy("/trunk/../a.txt", 6), None);
}

/// A report of `/trunk` at `from`, every path at that revision, asking for `to`.
fn trunk_report(from: Revision, to: Revision, send_copyfrom_args: bool) -> Report {
  Report {
    kind: RequestKind::Update,
    anchor: String::from("/trunk"),
    operand: String::new(),
    target_path: String::from("/trunk"),
    target_revision: to,
    source_revision: from,
    requested_depth: Depth::Unknown,
    text_deltas: true,
    send_copyfrom_args,
    ignore_ancestry: false,
    is_switch: false,
    target_info: PathInfo {
      path: String::new(),
      link_path: None,
      revision: Some(from),
      depth: Depth::Infinity,
      start_empty: false,
      lock_token: None,
    },
    paths: Vec::new(),
  }
}

/// A-616 part 2 (`add_file_smartly`, `delta_files`): drive `/trunk` from r3 to r4, where `new.txt`
/// moved to `renamed.txt`, once without copy sources and once with them; expect `renamed.txt` added
/// without a source and its whole text sent, then added from `/trunk/new.txt` at r3 with no text
/// sent, since its text is the source's; `new.txt` deleted in r4 both times.
#[test]
fn an_added_copy_names_its_source_when_the_client_asks() {
  let (index, green) = repository();
  let guards = FixtureGuards::anonymous();
  let calls = |copies| {
    run(
      &mut Drive::new(trunk_report(3, 4, copies), b"u"),
      &index,
      &green,
      &guards,
    )
  };
  let added = |calls: &[Written]| {
    calls
      .iter()
      .position(|call| matches!(call, Written::Add(_, path, _) if path == "renamed.txt"))
      .unwrap()
  };
  let plain = calls(false);
  let copied = calls(true);
  let (at_plain, at_copied) = (added(&plain), added(&copied));
  assert_eq!(
    plain[at_plain],
    Written::Add(NodeKind::File, "renamed.txt".into(), None)
  );
  let source = Some((String::from("/trunk/new.txt"), 3));
  assert_eq!(
    copied[at_copied],
    Written::Add(NodeKind::File, "renamed.txt".into(), source)
  );
  let base_of = |calls: &[Written], from| {
    calls[from..].iter().find_map(|call| match call {
      Written::Delta(base, _, _) => Some(*base),
      _ => None,
    })
  };
  assert_eq!(base_of(&plain, at_plain), Some(None));
  assert_eq!(base_of(&copied, at_copied), None);
  let deletion = Written::Delete(String::from("new.txt"), Some(4));
  assert!(plain.contains(&deletion) && copied.contains(&deletion));
}

/// A-616 part 2 (bounded work): drive the recorded checkout of the head with one step a call, then
/// two; expect the same calls as with steps to spare, and at least one pause each time.
#[test]
fn a_drive_paused_at_any_step_makes_the_same_calls() {
  let (index, green) = repository();
  let guards = FixtureGuards::anonymous();
  let session = SESSIONS
    .iter()
    .find(|session| session.name == "checkout-head.1")
    .unwrap();
  let whole = run(
    &mut Drive::new(finished_report(*session), b"u"),
    &index,
    &green,
    &guards,
  );
  for steps in 1..=2 {
    let mut drive = Drive::new(finished_report(*session), b"u");
    let (calls, pauses) = run_paused(&mut drive, &index, &green, &guards, steps);
    assert_eq!(calls, whole, "{steps} steps");
    assert!(pauses > 0, "{steps} steps never paused");
  }
}

/// A drive's calls with `steps` steps a call, resumed after every pause, and the pauses counted.
fn run_paused(
  drive: &mut Drive,
  index: &Index,
  green: &Green,
  guards: &FixtureGuards,
  steps: usize,
) -> (Vec<Written>, usize) {
  let mut calls = Vec::new();
  let mut pauses = 0;
  let mut delta: Option<(Option<[u8; 16]>, Vec<Window>)> = None;
  loop {
    match drive.next(index, green, guards, steps) {
      Next::Paused => pauses += 1,
      Next::Ended => return (calls, pauses),
      Next::Call(Call::ApplyTextDelta { base_md5 }) => delta = Some((base_md5, Vec::new())),
      Next::Call(Call::Window(window)) => delta.as_mut().unwrap().1.push(window.clone()),
      Next::Call(Call::TextDeltaEnd) => {
        let (base, windows) = delta.take().unwrap();
        calls.push(Written::Delta(base, 0, windows));
      }
      Next::Call(call) => calls.push(owned(call)),
    }
  }
}

/// The names a generated report's paths are made of: the fixtures' own, ones they never held, an
/// empty name and a parent's.
const NAMES: [&str; 9] = [
  "a.txt",
  "dir",
  "sub",
  "d.txt",
  "secret",
  "missing",
  "renamed.txt",
  "",
  "..",
];

/// The depths a generated report's paths take.
const DEPTHS: [Depth; 5] = [
  Depth::Exclude,
  Depth::Empty,
  Depth::Files,
  Depth::Immediates,
  Depth::Infinity,
];

/// One generated report path: its names, its revision (none for a deletion, past the youngest at
/// the top), its depth, whether it starts empty, whether it links, and whether it holds a token.
type GeneratedPath = (Vec<usize>, Option<u64>, usize, bool, bool, bool);

/// A generated report path made a report's path.
fn path_info((names, revision, depth, start_empty, link, token): &GeneratedPath) -> PathInfo {
  let path = names
    .iter()
    .map(|name| NAMES[*name % NAMES.len()])
    .collect::<Vec<_>>()
    .join("/");
  PathInfo {
    link_path: link.then(|| format!("/branches/b1/{path}")),
    path,
    revision: *revision,
    depth: DEPTHS[*depth % DEPTHS.len()],
    start_empty: *start_empty,
    lock_token: token.then(|| b"opaquelocktoken:held".to_vec()),
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

  /// A-616 part 2 (hostile reports): drive generated reports of `/trunk` (names held and not, empty
  /// and `..`, deletions, links, tokens, revisions past the youngest, every depth) at every target
  /// revision; expect each drive to end, never panic, and every call it makes taken in the editor's
  /// order by ra_svn's writer.
  #[test]
  fn every_drive_of_a_generated_report_ends_in_the_editors_order(
    from in 0u64..8,
    to in 0u64..=6,
    requested in 0usize..5,
    paths in proptest::collection::vec(
      (
        proptest::collection::vec(0usize..9, 0..4),
        proptest::option::of(0u64..8),
        0usize..5,
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
      ),
      0..8,
    ),
  ) {
    let (index, green) = repository();
    let guards = FixtureGuards::anonymous();
    let mut report = trunk_report(from, to, true);
    report.requested_depth = DEPTHS[requested];
    report.paths = paths.iter().map(path_info).collect();
    let mut drive = Drive::new(report, b"u");
    let capabilities = ClientCapabilities {
      svndiff1: true,
      accepts_svndiff2: true,
      absent_entries: true,
      depth: true,
      mergeinfo: true,
      log_revprops: true,
    };
    let mut writer = EditorWriter::new(&capabilities, MOST_OPEN);
    let mut out = Vec::new();
    let mut calls = 0usize;
    while let Next::Call(call) = drive.next(&index, &green, &guards, STEPS) {
      prop_assert_eq!(writer.write(&call, &mut out), Ok(()));
      calls += 1;
      prop_assert!(calls < STEPS, "the drive does not end");
    }
    prop_assert!(calls > 0);
  }
}
