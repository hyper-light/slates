//! The deltifier's tests (A-652): the windows' shape as `text_delta.c` cuts them, the instructions `xdelta.c` chooses in
//! cases worked by hand, and properties over generated texts: every delta remakes its target from its source with
//! svndiff's reference applier, keeps to its room, and survives version 0's encoding. The windows stock `svnadmin`
//! wrote are compared one by one in the dump codec's golden tests (`the_deltifier_makes_every_window_svnadmin_dump_
//! deltas_wrote`).

use super::*;
use crate::svndiff::{Compression, Encoder, Version, apply_all, decode_all};
use proptest::prelude::*;

/// A room of `count` bytes.
fn room(count: usize) -> NonZeroUsize {
  NonZeroUsize::new(count).unwrap()
}

/// The deltifier's windows of `target` against `source` within a room of `count` bytes.
fn windows(source: &[u8], target: &[u8], count: usize) -> Vec<Window> {
  deltas(source, target, room(count)).unwrap().collect()
}

/// Bytes that repeat no 64-byte block: a counter's low bytes, scrambled.
fn varied(count: usize, seed: u8) -> Vec<u8> {
  (0..count)
    .map(|place| {
      let place = u32::try_from(place).unwrap();
      (place
        .wrapping_mul(2_654_435_761)
        .wrapping_add(u32::from(seed))
        >> 13)
        .to_le_bytes()[0]
    })
    .collect()
}

/// The windows' views: each one's source offset, source length and target length.
fn views(windows: &[Window]) -> Vec<(u64, u64, u64)> {
  windows
    .iter()
    .map(|window| (window.source_offset, window.source_len, window.target_len))
    .collect()
}

/// A-652: do ask for windows of a room past `SVN_DELTA_WINDOW_SIZE`. Expect `WindowTooLarge`, since no stock decoder
/// takes them; the limit itself is taken.
#[test]
fn a_room_past_the_window_limit_is_refused() {
  let limit = usize::try_from(WINDOW_LIMIT).unwrap();
  assert_eq!(
    deltas(b"a", b"b", room(limit + 1)).err(),
    Some(SvndiffError::WindowTooLarge)
  );
  assert_eq!(windows(b"a", b"b", limit).len(), 1);
}

/// A-652 (`txdelta_next_window`): do delta an empty target, then a target of 25 bytes from a source of 13 within a
/// room of 10. Expect no window for the empty target; then three windows, the source viewed in step with the target
/// until it runs out (10 bytes, then 3), and after that an empty view starting where the source ended.
#[test]
fn the_source_is_viewed_in_step_with_the_target_until_it_runs_out() {
  assert!(windows(b"source", b"", 10).is_empty());
  let source = varied(13, 1);
  let target = varied(25, 2);
  assert_eq!(
    views(&windows(&source, &target, 10)),
    [(0, 10, 10), (10, 3, 10), (13, 0, 5)]
  );
  let whole = varied(20, 3);
  assert_eq!(
    views(&windows(&whole, &target, 10)),
    [(0, 10, 10), (10, 10, 10), (20, 0, 5)]
  );
}

/// A-652 (`compute_window`): do delta a text against the empty source. Expect every window the target's bytes as one
/// new-data instruction.
#[test]
fn an_empty_source_makes_the_target_new_data() {
  let target = varied(250, 4);
  let made = windows(b"", &target, 100);
  assert_eq!(made.len(), 3);
  for (window, piece) in made.iter().zip(target.chunks(100)) {
    assert_eq!(
      window.instructions,
      [Instruction::New {
        length: piece.len() as u64
      }]
    );
    assert_eq!(window.new_data, piece);
  }
}

/// A-652 (`compute_delta`, `store_delta_trailer`): do delta targets sharing four and then five bytes with the source at
/// their start and at their end. Expect four bytes kept as new data and five copied, as `xdelta.c` copies only a common
/// stretch longer than four bytes; and a target the source begins with copied whole.
#[test]
fn a_common_start_or_end_is_copied_only_past_four_bytes() {
  let source = b"abcdefghij0123456789";
  let four = windows(source, b"abcdXXXXXX6789", 64);
  assert_eq!(four[0].instructions, [Instruction::New { length: 14 }]);
  let five = windows(source, b"abcdeXXXXX56789", 64);
  assert_eq!(
    five[0].instructions,
    [
      Instruction::Source {
        offset: 0,
        length: 5
      },
      Instruction::New { length: 5 },
      Instruction::Source {
        offset: 15,
        length: 5
      },
    ]
  );
  let prefix = windows(source, b"abc", 64);
  assert_eq!(
    prefix[0].instructions,
    [Instruction::Source {
      offset: 0,
      length: 3
    }]
  );
}

/// A-652 (`compute_delta`'s match): do delta a target holding 128 bytes of the source's middle between bytes of its
/// own, whose edges differ from the source's bytes beside the stretch. Expect those bytes copied from where they stand in
/// the source, the match found at the source's whole block within them and extended back and forth to the stretch's
/// edges, the rest new data.
#[test]
fn a_moved_stretch_is_copied_from_where_it_stands_in_the_source() {
  let source = varied(400, 5);
  let mut before = varied(30, 6);
  before[0] = !source[0];
  before[29] = !source[99];
  let mut after = varied(30, 7);
  after[0] = !source[228];
  after[29] = !source[399];
  let target = [before.as_slice(), &source[100..228], after.as_slice()].concat();
  let made = windows(&source, &target, 1024);
  assert_eq!(
    made[0].instructions,
    [
      Instruction::New { length: 30 },
      Instruction::Source {
        offset: 100,
        length: 128
      },
      Instruction::New { length: 30 },
    ]
  );
  assert_eq!(apply_all(&source, &made).unwrap(), target);
}

/// The target of one generated edit of `source`: kept, cut, replaced by `bytes`, or moved.
fn edited(source: &[u8], edits: &[(u8, u16, u16, Vec<u8>)]) -> Vec<u8> {
  let mut target = source.to_vec();
  for (kind, at, length, bytes) in edits {
    let at = usize::from(*at) % (target.len() + 1);
    let end = (at + usize::from(*length)).min(target.len());
    match kind % 3 {
      0 => {
        target.splice(at..end, bytes.iter().copied());
      }
      1 => {
        let moved: Vec<u8> = target.drain(at..end).collect();
        target.extend(moved);
      }
      _ => {
        let copy: Vec<u8> = target[at..end].to_vec();
        target.splice(at..at, copy);
      }
    }
  }
  target
}

/// Holds windows to their room, their target to `target` through the reference applier, and their encoding in version 0
/// to decoding back to them.
fn holds(source: &[u8], target: &[u8], made: &[Window], count: usize) -> Result<(), TestCaseError> {
  for window in made {
    prop_assert!(window.target_len <= count as u64 && window.source_len <= count as u64);
  }
  let remade =
    apply_all(source, made).map_err(|refusal| TestCaseError::fail(format!("{refusal:?}")))?;
  prop_assert_eq!(remade, target.to_vec());
  let mut encoder = Encoder::new(Version::Zero);
  let mut bytes = encoder.header().to_vec();
  for window in made {
    encoder
      .window(window, Compression::Stored, &mut bytes)
      .map_err(|refusal| TestCaseError::fail(format!("{refusal:?}")))?;
  }
  let (_, decoded) =
    decode_all(&bytes).map_err(|refusal| TestCaseError::fail(format!("{refusal:?}")))?;
  prop_assert_eq!(decoded, made.to_vec());
  Ok(())
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

  /// A-652 (oracle): do delta generated targets (a source edited by replacements, cuts moved to the end, and copies of
  /// its own stretches) and unrelated ones, within rooms from one byte to past the texts. Expect every delta to remake
  /// its target from its source with svndiff's reference applier, every window within the room, and version 0's
  /// encoding to decode to the same windows.
  #[test]
  fn every_delta_remakes_its_target_within_its_room(
    source in prop::collection::vec(any::<u8>(), 0..700),
    edits in prop::collection::vec((any::<u8>(), any::<u16>(), 0u16..200, prop::collection::vec(any::<u8>(), 0..80)), 0..6),
    unrelated in prop::collection::vec(any::<u8>(), 0..300),
    count in 1usize..900,
  ) {
    let target = edited(&source, &edits);
    holds(&source, &target, &windows(&source, &target, count), count)?;
    holds(&source, &unrelated, &windows(&source, &unrelated, count), count)?;
    let repeated = [source.as_slice(), source.as_slice()].concat();
    holds(&source, &repeated, &windows(&source, &repeated, count), count)?;
  }
}
