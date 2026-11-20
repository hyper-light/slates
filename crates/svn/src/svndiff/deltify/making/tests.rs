//! The window maker's tests (A-751): the whole window is the oracle, [`super::super::deltas`]' window encoded by
//! [`crate::svndiff::Encoder::window`], itself held to the windows stock `svnadmin` wrote (the dump codec's golden
//! tests) and to svndiff's reference applier (`super::super::tests`). Every window is made under every unit size, in
//! version 0 and version 2 stored and LZ4, over texts made by hand and generated; and a large text's window is made a
//! few units a step with no step past its budget.

use super::*;
use crate::svndiff::{Encoder, Window, deltas};
use proptest::prelude::*;
use std::num::NonZeroUsize;

/// The encodings the tests make every window in.
const ENCODINGS: [(Version, Compression); 3] = [
  (Version::Zero, Compression::Stored),
  (Version::Two, Compression::Stored),
  (Version::Two, Compression::Lz4),
];

/// The deltifier's window of `target` against `source`, whole, and its encoding whole.
fn whole(source: &[u8], target: &[u8], (version, compression): (Version, Compression)) -> Vec<u8> {
  let room = NonZeroUsize::new(window_limit()).unwrap();
  let window = if source.is_empty() {
    Window {
      source_offset: 0,
      source_len: 0,
      target_len: target.len() as u64,
      instructions: if target.is_empty() {
        Vec::new()
      } else {
        vec![Instruction::New {
          length: target.len() as u64,
        }]
      },
      new_data: target.to_vec(),
    }
  } else {
    deltas(source, target, room).unwrap().next().unwrap()
  };
  let mut out = Vec::new();
  Encoder::new(version)
    .window(&window, compression, &mut out)
    .unwrap();
  out
}

/// The window made `work` units a step: its bytes and the steps it took.
fn made(
  source: &[u8],
  target: &[u8],
  (version, compression): (Version, Compression),
  work: usize,
) -> (Vec<u8>, usize) {
  let mut making =
    WindowMaking::new((0, source.len()), target.len(), version, compression).unwrap();
  let mut steps = 0usize;
  while !making.step(source, target, work).unwrap() {
    steps += 1;
  }
  (making.made().unwrap().bytes(), steps + 1)
}

/// The window limit as a length.
fn window_limit() -> usize {
  usize::try_from(WINDOW_LIMIT).unwrap()
}

/// Bytes that repeat no 64-byte block: a counter's low bytes, scrambled.
fn varied(count: usize, seed: u32) -> Vec<u8> {
  (0..count)
    .map(|place| {
      (u32::try_from(place & 0xFFFF_FFFF)
        .unwrap()
        .wrapping_mul(2_654_435_761)
        .wrapping_add(seed)
        >> 13)
        .to_le_bytes()[0]
    })
    .collect()
}

/// `source` edited as a commit edits a text: bytes changed every `every`, a run inserted and one removed.
fn edited(source: &[u8], every: usize) -> Vec<u8> {
  let mut target = source.to_vec();
  for place in (0..target.len()).step_by(every.max(1)) {
    target[place] = b'#';
  }
  let middle = target.len() / 2;
  target.splice(
    middle..middle,
    b"an inserted run of new bytes ".iter().copied(),
  );
  let quarter = target.len() / 4;
  target.drain(quarter..(quarter + 300).min(target.len()));
  target.truncate(window_limit());
  target
}

/// The texts made by hand: empty and short pieces, no source, equal texts, a common start and end alone, a source
/// repeated in the target, moved blocks, and a full window edited lightly and heavily.
fn cases() -> Vec<(Vec<u8>, Vec<u8>)> {
  let full = varied(window_limit(), 3);
  let mut moved = full[50_000..].to_vec();
  moved.extend_from_slice(&full[..50_000]);
  let mut repeated = varied(4_000, 8);
  repeated.extend(repeated.clone());
  repeated.extend(repeated.clone());
  vec![
    (Vec::new(), b"new text".to_vec()),
    (Vec::new(), varied(window_limit(), 1)),
    (b"abc".to_vec(), b"abd".to_vec()),
    (
      b"same prefix, other end".to_vec(),
      b"same prefix, else".to_vec(),
    ),
    (varied(3_000, 2), varied(3_000, 2)),
    (varied(5_000, 4), varied(5_000, 5)),
    (varied(100, 6), repeated.clone()),
    (repeated.clone(), repeated),
    (full.clone(), moved),
    (full.clone(), edited(&full, 997)),
    (full.clone(), edited(&full, 61)),
    (vec![b'a'; 20_000], vec![b'a'; 30_000]),
  ]
}

/// A-751. Do: make each hand-made window in each encoding under units from one up to the whole window. Expect: every
/// window's bytes the whole window's, and a full window's one-unit making many steps.
#[test]
fn every_window_is_the_whole_windows_bytes_under_any_unit() {
  for (index, (source, target)) in cases().iter().enumerate() {
    for encoding in ENCODINGS {
      let expected = whole(source, target, encoding);
      for work in [1, 3, 64, 257, 4_096, usize::MAX] {
        let (bytes, steps) = made(source, target, encoding, work);
        assert!(bytes == expected, "case {index}, {encoding:?}, work {work}");
        if work == 1 && target.len() >= 10_000 {
          assert!(steps > target.len() / 64, "case {index}: {steps} steps");
        }
      }
    }
  }
}

/// The units the red test gives each step: a few hundred, as a drive's step takes.
const STEP_WORK: usize = 256;

/// A-751, the red (main's: "a large text's window driven with the past-budget counter at 0, and the bytes equal to the
/// whole window's"), held by units, which contention cannot blur. Do: make a full 102,400-byte window of a text edited
/// every 997 bytes against its source, and one of a text no block of which its source holds (every look-up a miss), in
/// each encoding, [`STEP_WORK`] units a step. Expect: no step past [`STEP_WORK`] and [`STEP_OVERRUN`] units by the
/// making's own count, a step for every [`STEP_WORK`] bytes at least, and the bytes the whole window's. Red before the
/// change: the window was made in one call, its units the whole window's.
#[test]
fn a_large_texts_window_is_made_with_no_step_past_its_budget() {
  let source = varied(window_limit(), 11);
  for target in [edited(&source, 997), varied(window_limit(), 12)] {
    for encoding in ENCODINGS {
      let expected = whole(&source, &target, encoding);
      let mut making =
        WindowMaking::new((0, source.len()), target.len(), encoding.0, encoding.1).unwrap();
      let (mut steps, mut most) = (0usize, 0u64);
      loop {
        let before = making.units_taken();
        let done = making.step(&source, &target, STEP_WORK).unwrap();
        most = most.max(making.units_taken() - before);
        steps += 1;
        if done {
          break;
        }
      }
      assert!(
        most <= (STEP_WORK + STEP_OVERRUN) as u64,
        "{encoding:?}: a step of {most} units"
      );
      assert!(making.made().unwrap().bytes() == expected, "{encoding:?}");
      assert!(
        steps > target.len() / STEP_WORK,
        "{encoding:?}: {steps} steps"
      );
    }
  }
}

/// A-751. Do: step a making with pieces of other lengths than it was made for. Expect: refused, typed; a window past
/// the format's room refused at its making.
#[test]
fn pieces_it_was_not_made_for_are_refused() {
  let mut making = WindowMaking::new((0, 10), 10, Version::Zero, Compression::Stored).unwrap();
  assert_eq!(
    making.step(&[0; 9], &[0; 10], 1),
    Err(SvndiffError::CorruptWindowHeader)
  );
  assert!(
    WindowMaking::new(
      (0, 0),
      window_limit() + 1,
      Version::Zero,
      Compression::Stored
    )
    .is_err()
  );
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

  /// A-751. Do: make the window of a generated target against a generated source (from a small alphabet, so blocks
  /// repeat, and edits of the source) under a generated unit, in each encoding. Expect: the whole window's bytes.
  #[test]
  fn a_generated_window_is_the_whole_windows_bytes(
    source in proptest::strategy::Union::new([
      proptest::collection::vec(0u8..3, 0..2_000).boxed(),
      proptest::collection::vec(any::<u8>(), 0..2_000).boxed(),
    ]),
    edits in proptest::collection::vec((any::<usize>(), any::<u8>()), 0..20),
    tail in proptest::collection::vec(0u8..3, 0..300),
    work in 1usize..200,
  ) {
    let mut target = source.clone();
    for (place, byte) in edits {
      if !target.is_empty() {
        let at = place % target.len();
        target[at] = byte;
      }
    }
    target.extend(tail);
    for encoding in ENCODINGS {
      let (bytes, _) = made(&source, &target, encoding, work);
      prop_assert!(bytes == whole(&source, &target, encoding));
    }
  }
}
