//! A-703: the replay drive held to stock `svnserve` 1.15.0's own replays of the reads fixtures' repository
//! (`crates/svn/fixtures/replay`, recorded by its `make-fixtures.py`): every revision `svnsync synchronize` and
//! `svnrdump dump` replayed, and the scripted replays (a copy with and without deltas and below the low-water mark, a
//! replacement by a copy, revision 0, part of the history).

use super::{Replay, Stepped};
use crate::ra_svn::Value;
use crate::reads::tests::Fixture;
use crate::report::editor::EditorWriter;
use crate::report::recorded::{Written, command, items, read_drive, stock_capabilities};

/// Shape: the directories a replay of the fixtures holds open at once, past anything they hold.
const MOST_OPEN: usize = 64;

/// One recorded session: its name, the client's bytes and `svnserve`'s.
pub(crate) type Session = (&'static str, &'static [u8], &'static [u8]);

/// A recorded session by its name.
macro_rules! session {
  ($name:literal) => {
    (
      $name,
      include_bytes!(concat!(
        "../../../../fixtures/replay/",
        $name,
        ".1.client.bin"
      ))
      .as_slice(),
      include_bytes!(concat!(
        "../../../../fixtures/replay/",
        $name,
        ".1.server.bin"
      ))
      .as_slice(),
    )
  };
}

/// Every recorded session that replays.
pub(crate) const SESSIONS: [Session; 8] = [
  session!("svnsync-synchronize"),
  session!("svnrdump-dump"),
  session!("replay-copy"),
  session!("replay-copy-no-deltas"),
  session!("replay-copy-below-water"),
  session!("replay-replace"),
  session!("replay-zero"),
  session!("replay-range-part"),
];

/// A replay a recorded client asked for: each revision with its low-water mark and whether deltas go.
fn asked(client: &[u8]) -> Vec<(u64, u64, bool)> {
  let mut asked = Vec::new();
  for item in items(client) {
    match command(item) {
      Some((name, params)) if name == "replay" => {
        if let [
          Value::Number(revision),
          Value::Number(low),
          Value::Word(deltas),
        ] = params.as_slice()
        {
          asked.push((*revision, *low, *deltas == "true"));
        }
      }
      Some((name, params)) if name == "replay-range" => {
        if let [
          Value::Number(first),
          Value::Number(last),
          Value::Number(low),
          Value::Word(deltas),
        ] = params.as_slice()
        {
          asked.extend((*first..=*last).map(|revision| (revision, *low, *deltas == "true")));
        }
      }
      _ => {}
    }
  }
  asked
}

/// `svnserve`'s drives in `server`: each replayed revision's editor commands, up to its `finish-replay`.
pub(crate) fn recorded_drives(server: &[u8]) -> Vec<Vec<&[u8]>> {
  let mut drives = Vec::new();
  let mut drive: Option<Vec<&[u8]>> = None;
  for item in items(server) {
    let name = command(item).map(|(name, _)| name);
    match name.as_deref() {
      Some("target-rev") => drive = Some(vec![item]),
      Some("finish-replay") => drives.extend(drive.take()),
      _ => {
        if let Some(drive) = drive.as_mut() {
          drive.push(item);
        }
      }
    }
  }
  drives
}

/// Our replay of `revision`, written as the connection writes it for a stock 1.15 client.
fn replayed(fixture: &Fixture, (revision, low, deltas): (u64, u64, bool)) -> Vec<u8> {
  let (version, compression) = crate::report::editor::encoding_of(&stock_capabilities());
  let mut replay = Replay::new(revision, "/", low, deltas).with_encoding(version, compression);
  let mut writer = EditorWriter::new(&stock_capabilities(), MOST_OPEN);
  let mut out = Vec::new();
  while let Some(call) = replay.next(&fixture.index, &fixture.green, None).unwrap() {
    writer.write(&call, &mut out).unwrap();
  }
  out
}

/// A drive's calls, each node's property changes as a set: `svnserve` writes them in APR's hash order.
pub(crate) fn normalized(mut calls: Vec<Written>) -> Vec<Written> {
  let mut at = 0;
  while at < calls.len() {
    let end = calls[at..]
      .iter()
      .position(|call| !matches!(call, Written::ChangeProperty(..)))
      .map_or(calls.len(), |run| at + run);
    calls[at..end].sort_by(|left, right| format!("{left:?}").cmp(&format!("{right:?}")));
    at = end.max(at + 1);
  }
  calls
}

/// A-703 (`svn_repos_replay2` with `svn_delta_path_driver3`, `replay.c` and `path_driver.c` at 1.15.0). Do: for every
/// recorded session, replay each revision its client asked for over the reads fixtures' repository, at the client's
/// low-water mark and with or without deltas as it asked, and write the calls for a stock 1.15 client. Expect: each
/// revision's drive the calls `svnserve` wrote, call for call, each delta's windows decoded alike, each node's property
/// changes as a set; and as many drives as `svnserve` wrote.
#[test]
fn every_recorded_replay_is_driven_as_svnserve_drove_it() {
  let fixture = Fixture::new();
  let mut compared = 0;
  for (name, client, server) in SESSIONS {
    let theirs = recorded_drives(server);
    let ours: Vec<Vec<u8>> = asked(client)
      .into_iter()
      .map(|replay| replayed(&fixture, replay))
      .collect();
    assert_eq!(ours.len(), theirs.len(), "{name}: the revisions replayed");
    for (number, (ours, theirs)) in ours.iter().zip(&theirs).enumerate() {
      let ours = normalized(read_drive(&items(ours)).calls);
      let theirs = normalized(read_drive(theirs).calls);
      assert_eq!(ours, theirs, "{name}: replay {number}");
      compared += 1;
    }
  }
  // svnsync: revisions 1 to 11; svnrdump: 1 to 11; five single replays; the range 2 to 5.
  assert_eq!(compared, 11 + 11 + 5 + 4);
}

/// A-751, main's red for the replay drive ("a window in units through the same encoder, red first with the past-budget
/// counter at 0, bytes equal to today's"), held by units, which contention cannot blur. Do: over the report fixtures'
/// repository, replay r6, where `/trunk/big.txt` (6,000 lines, several windows) is edited, and r5, where it is added,
/// with deltas, in version 0 and in version 2 with LZ4, a step a call. Expect: no window step past `WINDOW_STEP_UNITS`
/// units, a step for every `STEP_WORK` bytes of the text at least, and `big.txt`'s delta the stream the whole-window
/// path made (`big_text_stream`), byte for byte. Red before the change: a window was made and given in one call.
#[test]
fn a_large_texts_replayed_windows_are_made_with_no_step_past_its_budget() {
  use crate::report::drive::tests::{big_text_stream, big_texts};
  use crate::report::drive::{Call, STEP_WORK, WINDOW_STEP_UNITS, take_most_window_units};
  use crate::svndiff::{Compression, Version};
  let (index, green, five, six) = big_texts();
  for (revision, base, text) in [(6, Some(five.clone()), &six), (5, None, &five)] {
    for (version, compression) in [
      (Version::Zero, Compression::Stored),
      (Version::Two, Compression::Lz4),
    ] {
      let expected = big_text_stream((base.as_deref(), text), (version, compression));
      let mut replay = Replay::new(revision, "/", 0, true).with_encoding(version, compression);
      let (mut most, mut window_steps) = (0usize, 0usize);
      let (mut streams, mut stream) = (Vec::new(), None::<Vec<u8>>);
      take_most_window_units();
      loop {
        let next = replay.next_within((&index, &green), None, 1).unwrap();
        let units = take_most_window_units();
        most = most.max(units);
        window_steps += usize::from(units > 0);
        match next {
          Stepped::Ended => break,
          Stepped::Call(Call::ApplyTextDelta { .. }) => stream = Some(Vec::new()),
          Stepped::Call(Call::DeltaPiece { bytes, .. }) => {
            stream.as_mut().unwrap().extend_from_slice(bytes);
          }
          Stepped::Call(Call::TextDeltaEnd) => streams.push(stream.take().unwrap()),
          _ => {}
        }
      }
      assert!(
        streams.contains(&expected),
        "r{revision}, {version:?}: big.txt's stream"
      );
      assert_eq!(
        (
          most <= WINDOW_STEP_UNITS,
          window_steps >= text.len() / STEP_WORK
        ),
        (true, true),
        "r{revision}, {version:?}: most units {most}, window steps {window_steps}"
      );
    }
  }
}

/// A history of `/trunk/big` holding 10,000 files (100 revisions of 100) and `/branches`, then one revision copying
/// `/trunk/big` to `/branches/big`: in `history.txt`'s grammar, the copy its last revision.
fn large_copy_history() -> String {
  let date = "2026-01-01T00:00:01.000000Z";
  let mut history =
    format!("\nrevision alice {date}\nmkdir trunk\nmkdir trunk/big\nmkdir branches\n");
  for revision in 0..100 {
    history.push_str(&format!("\nrevision alice {date}\n"));
    for file in 0..100 {
      let number = revision * 100 + file;
      history.push_str(&format!(
        "put trunk/big/f{number:05}.txt file {number}\\n\n"
      ));
    }
  }
  history.push_str(&format!(
    "\nrevision alice {date}\ncp 101 trunk/big branches/big\n"
  ));
  history
}

/// A-759, main's red for the replay drive's `add_subdir`: a copied directory listed through the index's cursor, held by
/// entries per step, which contention cannot blur. Do: replay the revision copying a 10,000-entry `/trunk/big` to
/// `/branches/big` with the low-water mark at that revision, so the copy is added whole, a step a call. Expect: no
/// step lists more than `LIST_STEP` entries, and the copy's additions are exactly the source's 10,000 names in order.
/// Red before the change: the copy's whole listing was one step.
#[test]
fn a_copied_large_directory_is_added_whole_a_few_entries_a_step() {
  use crate::report::drive::green::repository_from;
  use crate::report::drive::{Call, LIST_STEP, take_most_listed};
  let (index, green) = repository_from(&large_copy_history());
  let copy = index.youngest();
  let names: Vec<String> = index
    .children("/trunk/big", copy - 1)
    .unwrap()
    .unwrap()
    .into_iter()
    .map(|name| format!("branches/big/{name}"))
    .collect();
  assert_eq!(names.len(), 10_000);
  let mut replay = Replay::new(copy, "/", copy, false);
  let (mut most, mut added) = (0usize, Vec::new());
  take_most_listed();
  loop {
    let next = replay.next_within((&index, &green), None, 1).unwrap();
    most = most.max(take_most_listed());
    match next {
      Stepped::Ended => break,
      Stepped::Call(Call::Add { path, .. }) if path.starts_with("branches/big/") => {
        added.push(path.to_owned());
      }
      _ => {}
    }
  }
  assert!(
    added == names,
    "the copy's additions: {} of {}",
    added.len(),
    names.len()
  );
  assert!(most <= LIST_STEP, "a step listed {most}");
}
