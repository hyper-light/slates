//! A green's checkpoint (§4.16 "Deltas older than the derived retention fold into checkpoint deltas … exact"; W3 of the
//! load test's walls): restored from its origin and a checkpoint at `F`, then given every increment accepted after `F`,
//! a green answers exactly as the green the checkpoint was taken from for every version from `F` on: its head, its head
//! identity, every version's files, and the verdict on any further increment based at or after `F`. A base below `F` is
//! refused, never judged, and a retry of an increment accepted at or before `F` is answered from the checkpoint. The
//! codec round-trips and refuses hostile bytes typed. Oracle: the original green, over generated origins and chains of
//! every dimension (the shared generators of `tests/identity.rs` and `tests/base_for.rs`).
// Test harness code: an unwrap or a panic in a helper is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

mod common;

use common::generated::{Raw, increment_of, origin_of, raw};
use proptest::prelude::*;
use slates_merge::checkpoint::{Changed, Checkpoint};
use slates_merge::engine::{Green, Increment, Outcome};
use slates_merge::ops_doc::DocDecodeError;

/// A distinct identity for the chain's `n`th increment.
fn id(n: usize, tag: u8) -> [u8; 32] {
  let mut id = [tag; 32];
  id[..8].copy_from_slice(&(n as u64).to_le_bytes());
  id
}

/// Builds a green from `origin_rows` and submits `chain` at the head: the green and the increments it accepted, each
/// with its version.
fn chain_of(origin_rows: &[Raw], chain: &[Raw]) -> (Green, Vec<(Increment, u64)>) {
  let mut green = Green::with_origin(origin_of(origin_rows));
  let mut accepted = Vec::new();
  for (n, step) in chain.iter().enumerate() {
    let increment = increment_of(step).with_id(id(n, 0xA0), green.head());
    if let Outcome::Accepted { version } = green.submit(&increment) {
      accepted.push((increment, version));
    }
  }
  (green, accepted)
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// W3, §4.16. Do: build a green from a generated origin and chain; optionally fold its histories below `F`; take its
  /// checkpoint at `F`; restore a green from the origin and the checkpoint and replay every increment accepted after
  /// `F`; submit one more generated increment based in `[F, head]` to both. Expect: the checkpoint round-trips through
  /// its encoding; the two greens have one head and one head identity (and the restored one's equals its identity from
  /// scratch); they hold the same files at every version from `F` to the head; they give the further increment the same
  /// verdict; a base below `F` is refused `BaseUnavailable` by the restored green; and a retry of an increment accepted
  /// at or before `F` is answered with its version.
  #[test]
  fn a_green_restored_from_its_checkpoint_answers_as_the_green_it_was(
    origin_rows in prop::collection::vec(raw(), 0..12),
    chain in prop::collection::vec(raw(), 1..24),
    extra in raw(),
    pick in any::<u16>(),
    pick_base in any::<u16>(),
    fold in any::<bool>(),
  ) {
    let (mut green, accepted) = chain_of(&origin_rows, &chain);
    let head = green.head();
    let at = u64::from(pick) % (head + 1);
    if fold {
      green.fold_history_before(at);
    }
    let checkpoint = green.checkpoint(at).expect("a checkpoint at or below the head, at or above the floor");
    prop_assert_eq!(&Checkpoint::decode(&checkpoint.encode()).unwrap(), &checkpoint);
    let mut restored = Green::restore(origin_of(&origin_rows), &checkpoint);
    prop_assert_eq!(restored.head(), at);
    for (increment, version) in accepted.iter().filter(|(_, version)| *version > at) {
      prop_assert_eq!(restored.submit(increment), Outcome::Accepted { version: *version });
    }
    prop_assert_eq!(restored.head(), head);
    prop_assert_eq!(restored.head_identity(), green.head_identity());
    prop_assert_eq!(restored.head_identity(), restored.head_identity_from_scratch());
    for version in at..=head {
      prop_assert_eq!(restored.files_at(version), green.files_at(version), "files at {}", version);
    }
    // What changed since any version, below the checkpoint and the fold too, is each path whose last change is after
    // it: the change index answers it as the histories do, in the green and in its restoration (A-264).
    for version in 0..=head {
      let expected = green.changed_between(version, head);
      prop_assert_eq!(&green.changed_since(version), &expected, "changed since {}", version);
      prop_assert_eq!(&restored.changed_since(version), &expected, "restored, changed since {}", version);
    }
    prop_assert_eq!(restored.changes_recounted(), restored.changes_bytes());
    if let Some((increment, version)) = accepted.iter().find(|(_, version)| *version <= at) {
      prop_assert_eq!(restored.submit(increment), Outcome::Accepted { version: *version });
    }
    if at > 0 {
      let below = increment_of(&extra).with_id(id(0, 0xB0), at - 1);
      prop_assert_eq!(
        restored.submit(&below),
        Outcome::BaseUnavailable { base: at - 1, floor: at }
      );
    }
    let base = at + u64::from(pick_base) % (head - at + 1);
    let further = increment_of(&extra).with_id(id(0, 0xC0), base);
    let original = green.submit(&further);
    prop_assert_eq!(restored.submit(&further), original);
    prop_assert_eq!(restored.head_identity(), green.head_identity());
  }
}

/// A checkpoint touching every table, for the codec's tests.
fn every_table() -> Checkpoint {
  Checkpoint {
    head_check: Some((9, [4; 32])),
    version: 7,
    accepted: vec![([3; 32], 2), ([1; 32], 5)],
    files: vec![
      Changed {
        key: "b".to_owned(),
        at: 5,
        value: Some(b"bytes".to_vec()),
      },
      Changed {
        key: "a".to_owned(),
        at: 3,
        value: None,
      },
    ],
    dirs: vec![Changed {
      key: "d".to_owned(),
      at: 2,
      value: Some(()),
    }],
    modes: vec![Changed {
      key: "b".to_owned(),
      at: 6,
      value: Some(0o640),
    }],
    symlinks: vec![Changed {
      key: "s".to_owned(),
      at: 4,
      value: Some("b".to_owned()),
    }],
    hardlinks: vec![Changed {
      key: "h".to_owned(),
      at: 7,
      value: None,
    }],
    specials: vec![Changed {
      key: "f".to_owned(),
      at: 1,
      value: Some(common::generated::node(3)),
    }],
    xattrs: vec![Changed {
      key: ("b".to_owned(), "user.k".to_owned()),
      at: 6,
      value: Some(vec![9]),
    }],
  }
}

/// Determinism gate. Do: encode one checkpoint twice, once with its tables declared in another order. Expect: the same
/// bytes, and its decode equal to the canonical form.
#[test]
fn a_checkpoint_encodes_to_one_byte_sequence_whatever_its_order() {
  let checkpoint = every_table();
  let mut reordered = checkpoint.clone();
  reordered.files.reverse();
  reordered.accepted.reverse();
  assert_eq!(checkpoint.encode(), reordered.encode());
  let mut canonical = checkpoint.clone();
  canonical.canonicalize();
  assert_eq!(Checkpoint::decode(&checkpoint.encode()).unwrap(), canonical);
}

/// Hostile input. Do: decode every proper prefix of a valid checkpoint, a foreign magic, a newer version, a count no
/// remaining bytes could hold, a presence byte that is neither, and a valid encoding with a byte appended. Expect: each
/// refused typed, never a panic, never a checkpoint.
#[test]
fn a_hostile_checkpoint_is_refused_typed() {
  let bytes = every_table().encode();
  for cut in 0..bytes.len() {
    assert!(
      Checkpoint::decode(&bytes[..cut]).is_err(),
      "a prefix of {cut} bytes was decoded"
    );
  }
  let mut foreign = bytes.clone();
  foreign[0] ^= 0xFF;
  assert_eq!(Checkpoint::decode(&foreign), Err(DocDecodeError::BadMagic));
  let mut newer = bytes.clone();
  newer[4] = newer[4].wrapping_add(1);
  assert_eq!(Checkpoint::decode(&newer), Err(DocDecodeError::BadVersion));
  let mut huge = bytes[..16].to_vec();
  huge.extend_from_slice(&u32::MAX.to_le_bytes());
  assert_eq!(Checkpoint::decode(&huge), Err(DocDecodeError::Truncated));
  let empty_tables = Checkpoint {
    version: 1,
    files: vec![Changed {
      key: "a".to_owned(),
      at: 1,
      value: None,
    }],
    ..Checkpoint::default()
  }
  .encode();
  let mut presence = empty_tables.clone();
  // The files table's one entry ends with its presence byte, after its key and version; the six empty tables and the
  // head check's own presence byte follow.
  let presence_at = empty_tables.len() - 1 - 1 - 6 * size_of::<u32>();
  presence[presence_at] = 7;
  assert_eq!(Checkpoint::decode(&presence), Err(DocDecodeError::BadKind));
  let mut head_check = empty_tables.clone();
  *head_check.last_mut().unwrap() = 7;
  assert_eq!(
    Checkpoint::decode(&head_check),
    Err(DocDecodeError::BadKind)
  );
  let mut trailing = bytes;
  trailing.push(0);
  assert_eq!(
    Checkpoint::decode(&trailing),
    Err(DocDecodeError::TrailingBytes)
  );
}
