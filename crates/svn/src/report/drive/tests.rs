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

use super::green::{
  CHANGED, Green, NOW, locks_of, md5, normalize, repository, rules_of, texts_by_md5,
};
use super::*;
use crate::handshake::ClientCapabilities;
use crate::lock::LockTable;
use crate::ra_svn::Value;
use crate::report::RequestKind;
use crate::report::editor::EditorWriter;
use crate::report::recorded::{
  SESSIONS, Written, YOUNGEST, command, drive_items, finished_report, items, read_drive,
};
use proptest::prelude::*;

/// The steps a test gives each call: past any one call's walk in the fixtures.
const STEPS: usize = 1 << 20;

/// The drives the replay test must compare: the 22 stock sessions' and seven hand-made ones' (the
/// early answer's is the handler's).
const COMPARED_DRIVES: usize = 29;

/// The most nodes a writer checking a generated drive holds open: past any path a report names.
const MOST_OPEN: usize = 64;

/// A drive's calls, owned as a recording's are, its reads judged by the anonymous principal's
/// rules.
fn run(drive: &mut Drive, index: &Index, green: &Green, locks: &LockTable) -> Vec<Written> {
  let mut calls = Vec::new();
  let mut delta: Option<(Option<[u8; 16]>, Vec<u8>)> = None;
  let anonymous = rules_of(None);
  let sources = Sources {
    index,
    repository: green,
    locks,
    now: NOW,
    rules: Some(&anonymous),
  };
  loop {
    let call = match drive.next(&sources, STEPS) {
      Next::Call(call) => call,
      Next::Paused => panic!("a call needed more steps than {STEPS}"),
      Next::Ended => return calls,
    };
    match call {
      Call::ApplyTextDelta { base_md5 } => delta = Some((base_md5, Vec::new())),
      Call::DeltaPiece { bytes, .. } => delta.as_mut().unwrap().1.extend_from_slice(bytes),
      Call::TextDeltaEnd => {
        let (base, stream) = delta.take().unwrap();
        calls.push(Written::Delta(base, 0, windows_of(&stream)));
      }
      other => calls.push(owned(other)),
    }
  }
}

/// The windows of a delta's svndiff stream as the drive sent it in pieces (A-751): none for a delta
/// with no window, whose stream its writer ends with the header alone.
fn windows_of(stream: &[u8]) -> Vec<Window> {
  if stream.is_empty() {
    return Vec::new();
  }
  crate::svndiff::decode_all(stream).unwrap().1
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
    let locks = locks_of(session.name, &report);
    let mut drive = Drive::new(report, b"00000000-0000-4000-8000-0000000a0616");
    let ours = run(&mut drive, &index, &green, &locks);
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
  let locks = LockTable::new();
  let calls = |copies| {
    run(
      &mut Drive::new(trunk_report(3, 4, copies), b"u"),
      &index,
      &green,
      &locks,
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
  let locks = LockTable::new();
  let session = SESSIONS
    .iter()
    .find(|session| session.name == "checkout-head.1")
    .unwrap();
  let whole = run(
    &mut Drive::new(finished_report(*session), b"u"),
    &index,
    &green,
    &locks,
  );
  for steps in 1..=2 {
    let mut drive = Drive::new(finished_report(*session), b"u");
    let (calls, pauses) = run_paused(&mut drive, &index, &green, &locks, steps);
    assert_eq!(calls, whole, "{steps} steps");
    assert!(pauses > 0, "{steps} steps never paused");
  }
}

/// A drive's calls with `steps` steps a call, resumed after every pause, and the pauses counted,
/// its reads judged by the anonymous principal's rules.
fn run_paused(
  drive: &mut Drive,
  index: &Index,
  green: &Green,
  locks: &LockTable,
  steps: usize,
) -> (Vec<Written>, usize) {
  let mut calls = Vec::new();
  let mut pauses = 0;
  let mut delta: Option<(Option<[u8; 16]>, Vec<u8>)> = None;
  let anonymous = rules_of(None);
  let sources = Sources {
    index,
    repository: green,
    locks,
    now: NOW,
    rules: Some(&anonymous),
  };
  loop {
    match drive.next(&sources, steps) {
      Next::Paused => pauses += 1,
      Next::Ended => return (calls, pauses),
      Next::Call(Call::ApplyTextDelta { base_md5 }) => delta = Some((base_md5, Vec::new())),
      Next::Call(Call::DeltaPiece { bytes, .. }) => {
        delta.as_mut().unwrap().1.extend_from_slice(bytes);
      }
      Next::Call(Call::TextDeltaEnd) => {
        let (base, stream) = delta.take().unwrap();
        calls.push(Written::Delta(base, 0, windows_of(&stream)));
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
    let locks = LockTable::new();
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
    let anonymous = rules_of(None);
    let sources = Sources {
      index: &index,
      repository: &green,
      locks: &locks,
      now: NOW,
      rules: Some(&anonymous),
    };
    while let Next::Call(call) = drive.next(&sources, STEPS) {
      prop_assert_eq!(writer.write(&call, &mut out), Ok(()));
      calls += 1;
      prop_assert!(calls < STEPS, "the drive does not end");
    }
    prop_assert!(calls > 0);
  }
}

/// A-616 part 2 with A-652 (`delta_files` through Subversion's deltifier): drive `/trunk` from r5
/// to r6, where one line of `big.txt`'s 6,000 changed; expect its delta against the r5 text's MD5,
/// windows that copy from that text and carry far less new data than the text, and the r6 text
/// made of them.
#[test]
fn a_changed_text_is_sent_as_a_delta_against_its_base() {
  let (index, green) = repository();
  let locks = LockTable::new();
  let mut drive = Drive::new(trunk_report(5, 6, false), b"u");
  let calls = run(&mut drive, &index, &green, &locks);
  let at = calls
    .iter()
    .position(|call| matches!(call, Written::Open(NodeKind::File, path, _) if path == "big.txt"))
    .unwrap();
  let Some(Written::Delta(base, _, windows)) = calls[at..]
    .iter()
    .find(|call| matches!(call, Written::Delta(..)))
  else {
    panic!("big.txt sent no delta");
  };
  let texts = texts_by_md5(&green);
  let source = &texts[&base.unwrap()];
  let target = crate::svndiff::apply_all(source, windows).unwrap();
  let copies = windows
    .iter()
    .flat_map(|window| &window.instructions)
    .filter(|instruction| matches!(instruction, crate::svndiff::Instruction::Source { .. }))
    .count();
  let new_data: usize = windows.iter().map(|window| window.new_data.len()).sum();
  assert!(copies > 0, "no window copied from the base");
  assert!(
    new_data * 100 < target.len(),
    "{new_data} new bytes of {}",
    target.len()
  );
  let closed = calls[at..].iter().find_map(|call| match call {
    Written::CloseFile(md5) => Some(*md5),
    _ => None,
  });
  assert_eq!(
    Some(md5(&target)),
    closed,
    "the text made is not the one closed"
  );
}

/// The svndiff stream the whole-window path made of `/trunk/big.txt` from its text at `from` to its text at `to`
/// (`read_window`, `window_of`, `Encoder::window`): the bytes a drive's pieces must make.
pub(crate) fn big_text_stream(
  (from, to): (Option<&[u8]>, &[u8]),
  (version, compression): (Version, Compression),
) -> Vec<u8> {
  let mut encoder = Encoder::new(version);
  let mut stream = encoder.header().to_vec();
  let mut sent = 0usize;
  while sent < to.len() {
    let target = &to[sent..(sent + usize::try_from(WINDOW_LIMIT).unwrap()).min(to.len())];
    let source = from.map_or(&[][..], |from| {
      let view = sent.min(from.len());
      &from[view..(view + usize::try_from(WINDOW_LIMIT).unwrap()).min(from.len())]
    });
    let mut window = window_of(source, target).unwrap();
    window.source_offset = sent.min(from.map_or(0, <[u8]>::len)) as u64;
    encoder.window(&window, compression, &mut stream).unwrap();
    sent += target.len();
  }
  stream
}

/// A-751, main's red at the drive ("a large text's window driven with the past-budget counter at 0, and the bytes equal
/// to the whole window's"), held by units, which contention cannot blur: drive `/trunk` from r5 to r6, where `big.txt`
/// (6,000 lines, several windows) is edited, and from r4, where it is added, in version 0 and in version 2 with LZ4, a
/// step a call. Expect: no window step past [`WINDOW_STEP_UNITS`] units (bytes read or sent, or the maker's units),
/// at least a step for every [`STEP_WORK`] bytes of the text, no piece past [`STEP_WORK`] bytes, and `big.txt`'s delta the
/// stream the whole-window path made, byte for byte. Red before the change: a window was made and written in one step,
/// its units the whole window's. Its time in release is read by `window_step_times` and the diagnostics runs.
#[test]
fn a_large_texts_windows_are_driven_with_no_step_past_its_budget() {
  let (index, green, five, six) = big_texts();
  let locks = LockTable::new();
  let sources = Sources {
    index: &index,
    repository: &green,
    locks: &locks,
    now: NOW,
    rules: None,
  };
  for (from, base) in [(5, Some(five.clone())), (4, None)] {
    for encoding in [
      (Version::Zero, Compression::Stored),
      (Version::Two, Compression::Lz4),
    ] {
      let expected = big_text_stream((base.as_deref(), &six), encoding);
      let mut drive =
        Drive::new(trunk_report(from, 6, false), b"u").with_encoding(encoding.0, encoding.1);
      let (mut most, mut window_steps, mut largest_piece) = (0usize, 0usize, 0usize);
      let (mut streams, mut stream) = (Vec::new(), None::<Vec<u8>>);
      take_most_window_units();
      loop {
        let next = drive.next(&sources, 1);
        let units = take_most_window_units();
        most = most.max(units);
        window_steps += usize::from(units > 0);
        match next {
          Next::Ended => break,
          Next::Call(Call::ApplyTextDelta { .. }) => stream = Some(Vec::new()),
          Next::Call(Call::DeltaPiece { bytes, .. }) => {
            largest_piece = largest_piece.max(bytes.len());
            stream.as_mut().unwrap().extend_from_slice(bytes);
          }
          Next::Call(Call::TextDeltaEnd) => streams.push(stream.take().unwrap()),
          _ => {}
        }
      }
      assert!(
        streams.contains(&expected),
        "r{from}, {encoding:?}: big.txt's stream"
      );
      assert_eq!(
        (
          most <= WINDOW_STEP_UNITS,
          largest_piece <= STEP_WORK,
          window_steps >= six.len() / STEP_WORK
        ),
        (true, true, true),
        "r{from}, {encoding:?}: most units {most}, largest piece {largest_piece}, window steps {window_steps}"
      );
    }
  }
}

/// A-751's by-use signal in release (`cargo test --release -p slates-svn --lib -- --ignored window_step_times
/// --nocapture`): the drive of `big.txt` from r5 to r6 a step a call, each call's wall time, printed: the longest and
/// the calls past the measured quantum's order (5 us). Measured, not asserted: a timing under contention is the
/// machine's as much as the step's.
#[test]
#[ignore = "a measurement, run in release"]
fn window_step_times() {
  let (index, green, _, _) = big_texts();
  let locks = LockTable::new();
  let sources = Sources {
    index: &index,
    repository: &green,
    locks: &locks,
    now: NOW,
    rules: None,
  };
  let mut drive = Drive::new(trunk_report(5, 6, false), b"u");
  let (mut calls, mut past, mut longest) = (0usize, 0usize, std::time::Duration::ZERO);
  loop {
    let began = std::time::Instant::now();
    let ended = matches!(drive.next(&sources, 1), Next::Ended);
    let took = began.elapsed();
    calls += 1;
    longest = longest.max(took);
    past += usize::from(took > std::time::Duration::from_micros(5));
    if ended {
      break;
    }
  }
  eprintln!("window_step_times: {calls} calls, {past} past 5 us, longest {longest:?}");
}

/// The report fixtures' repository and `/trunk/big.txt`'s text where it is added (r5) and where it is edited (r6), for
/// the window tests of the other readers (A-751).
pub(crate) fn big_texts() -> (Index, Green, Vec<u8>, Vec<u8>) {
  let (index, green) = repository();
  let locks = LockTable::new();
  let (five, six) = {
    let sources = Sources {
      index: &index,
      repository: &green,
      locks: &locks,
      now: NOW,
      rules: None,
    };
    let text_at = |revision: Revision| {
      let text = dirent(&sources, "/trunk/big.txt", revision)
        .unwrap()
        .unwrap()
        .text
        .unwrap();
      green.texts[&text.identity].clone()
    };
    (text_at(5), text_at(6))
  };
  (index, green, five, six)
}

/// A history of one directory, `/trunk/big`, gaining `per_revision` files in each of `revisions` revisions, the first
/// file of the revision before deleted and its last edited in each: a large directory with a long history, in
/// `history.txt`'s grammar.
pub(crate) fn long_directory_history(revisions: usize, per_revision: usize) -> String {
  let date = "2026-01-01T00:00:01.000000Z";
  let mut history = format!("\nrevision alice {date}\nmkdir trunk\nmkdir trunk/big\n");
  for revision in 0..revisions {
    history.push_str(&format!("\nrevision alice {date}\n"));
    for file in 0..per_revision {
      let number = revision * per_revision + file;
      history.push_str(&format!(
        "put trunk/big/f{number:07}.txt file {number}\\n\n"
      ));
    }
    if revision > 0 {
      let first = (revision - 1) * per_revision;
      let last = first + per_revision - 1;
      history.push_str(&format!("rm trunk/big/f{first:07}.txt\n"));
      history.push_str(&format!(
        "put trunk/big/f{last:07}.txt edited in {revision}\\n\n"
      ));
    }
  }
  history
}

/// A-751 follow-up's A/B (main's: "listing time ... before and after, on a large directory with a long history"), in
/// release (`cargo test --release -p slates-svn --lib -- --ignored listing_times --nocapture`): `/trunk/big` after
/// 1,000 revisions of 10 files each (9,001 live, 10,000 named), listed at the youngest revision and at the middle one.
/// Printed for each: `Index::children`; a dirent resolved for every child listed (`history::present`, as the drive's
/// `entries` made one before A-759); and the drive's `entries` as it is now. Measured, not asserted.
#[test]
#[ignore = "a measurement, run in release"]
fn listing_times() {
  let (index, green) = green::repository_from(&long_directory_history(1_000, 10));
  let locks = LockTable::new();
  let sources = Sources {
    index: &index,
    repository: &green,
    locks: &locks,
    now: NOW,
    rules: None,
  };
  let youngest = index.youngest();
  eprintln!(
    "listing_times: the index holds {} entries of {} bytes ({} bytes), the children maps' bytes among them",
    index.entries(),
    Index::ENTRY_BYTES,
    index.entries() * Index::ENTRY_BYTES
  );
  for revision in [youngest, youngest / 2] {
    let best = |run: &dyn Fn() -> usize| {
      (0..5)
        .map(|_| {
          let began = std::time::Instant::now();
          let count = run();
          (began.elapsed(), count)
        })
        .min()
        .unwrap()
    };
    let (children, live) = best(&|| {
      index
        .children("/trunk/big", revision)
        .unwrap()
        .unwrap()
        .len()
    });
    let names = index.children("/trunk/big", revision).unwrap().unwrap();
    let (dirents, _) = best(&|| {
      names
        .iter()
        .filter(|name| {
          dirent(&sources, &join_fspath("/trunk/big", name), revision)
            .unwrap()
            .is_some()
        })
        .count()
    });
    let (entries_made, _) = best(&|| entries(&sources, "/trunk/big", revision).unwrap().len());
    eprintln!(
      "listing_times r{revision}: {live} live; children {children:?}; dirents resolved {dirents:?}; the drive's \
       entries (listing, each from its row) {entries_made:?}"
    );
  }
}
