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
/// counter at 0, bytes equal to today's"). Do: over the report fixtures' repository, replay r6, where `/trunk/big.txt`
/// (6,000 lines, several windows) is edited, and r5, where it is added, with deltas, in version 0 and in version 2
/// with LZ4, a step a call, timing each call (a call past the budget timed again from its state before,
/// `RETIMES`). Expect: no call past `STEP_BUDGET`; `big.txt`'s delta the stream the whole-window path made
/// (`big_text_stream`), byte for byte. Red before the change: a window was made and given in one call, milliseconds in
/// a debug build.
#[test]
fn a_large_texts_replayed_windows_are_made_with_no_step_past_its_budget() {
  use crate::report::drive::green::repository;
  use crate::report::drive::tests::{RETIMES, STEP_BUDGET, big_text_stream};
  use crate::report::drive::{Call, Sources};
  use crate::svndiff::{Compression, Version};
  use std::time::{Duration, Instant};
  let (index, green) = repository();
  let locks = crate::lock::LockTable::new();
  let sources = Sources {
    index: &index,
    repository: &green,
    locks: &locks,
    now: crate::lock::AprTime(0),
    rules: None,
  };
  let text_at = |revision| {
    let text = super::super::dirent(&sources, "/trunk/big.txt", revision)
      .unwrap()
      .unwrap()
      .text
      .unwrap();
    green.texts[&text.identity].clone()
  };
  for (revision, base) in [(6, Some(text_at(5))), (5, None)] {
    for (version, compression) in [
      (Version::Zero, Compression::Stored),
      (Version::Two, Compression::Lz4),
    ] {
      let expected = big_text_stream(
        (base.as_deref(), &text_at(revision)),
        (version, compression),
      );
      let mut replay = Replay::new(revision, "/", 0, true).with_encoding(version, compression);
      let (mut past, mut longest, mut streams) = (0usize, Duration::ZERO, Vec::new());
      let mut stream: Option<Vec<u8>> = None;
      loop {
        let before = replay.clone();
        let began = Instant::now();
        let next = replay.next_within((&index, &green), None, 1).unwrap();
        let mut took = began.elapsed();
        for _ in 0..RETIMES {
          if took <= STEP_BUDGET {
            break;
          }
          let mut again = before.clone();
          let began = Instant::now();
          let _ = again.next_within((&index, &green), None, 1).unwrap();
          took = took.min(began.elapsed());
        }
        longest = longest.max(took);
        past += usize::from(took > STEP_BUDGET);
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
        past, 0,
        "r{revision}, {version:?}: longest step {longest:?}"
      );
    }
  }
}
