//! The memory partition's tests (A-846, A-724 piece 2): the floor, optimal on straight curves against brute force, a
//! scan's step not starved, and room nobody wants kept spare.
#![allow(
  clippy::unwrap_used,
  clippy::indexing_slicing,
  clippy::cast_precision_loss,
  clippy::arithmetic_side_effects
)]

use super::{Want, partition};
use crate::mrc::Curve;

/// Shape: the window the tests count in, in bytes.
const GRANULE: u64 = 4096;

fn key_of(object: u64) -> u64 {
  let digest = blake3::hash(&object.to_le_bytes());
  u64::from_le_bytes(digest.as_bytes()[..8].try_into().unwrap())
}

/// A curve from `reads` reads drawn uniformly over `objects` one-window objects (offset by `salt`, so curves never
/// share keys): its misses fall in a straight line until it holds them all.
fn uniform(objects: u64, reads: usize, salt: u64) -> Curve {
  let mut curve = Curve::new(1 << 16, GRANULE);
  let mut state = 0x2545_F491_4F6C_DD1Du64 ^ salt;
  for _ in 0..reads {
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    curve.read(key_of(salt << 32 | state % objects), GRANULE);
  }
  curve
}

/// A curve from `loops` scans over `objects` one-window objects: every read misses until all of them fit, then none.
fn scan(objects: u64, loops: u64, salt: u64) -> Curve {
  let mut curve = Curve::new(1 << 16, GRANULE);
  for _ in 0..loops {
    for object in 0..objects {
      curve.read(key_of(salt << 32 | object), GRANULE);
    }
  }
  curve
}

/// Misses a second of a division.
fn cost(wants: &[Want<'_>], given: &[u64]) -> f64 {
  wants
    .iter()
    .zip(given)
    .map(|(want, bytes)| want.curve.misses(*bytes) * want.per_s)
    .sum()
}

/// A-846 ("The floor"). Do: divide 1,000 windows among four repositories, one with a working set past the whole room,
/// three of 20 to 80 objects. Expect: each given at least the lesser of 250 windows and its working set (the small ones
/// whole), the large one the rest, and the whole given and spare no more than the room.
#[test]
fn every_repository_keeps_its_floor() {
  let curves = [
    uniform(5_000, 40_000, 1),
    uniform(50, 2_000, 2),
    uniform(80, 2_000, 3),
    uniform(20, 2_000, 4),
  ];
  let wants: Vec<Want<'_>> = curves
    .iter()
    .map(|curve| Want { curve, per_s: 1.0 })
    .collect();
  let room = 1_000 * GRANULE;
  let division = partition(room, &wants);
  for (curve, bytes) in curves.iter().zip(&division.given) {
    assert!(
      *bytes >= (250 * GRANULE).min(curve.working_set()),
      "{division:?}"
    );
  }
  assert!(
    division.given[0] >= room - (50 + 80 + 20) * 2 * GRANULE,
    "{division:?}"
  );
  assert!(division.given.iter().sum::<u64>() + division.spare <= room);
}

/// A-846 (Qureshi and Patt's partition, optimal on convex curves). Do: divide 600 windows among two repositories
/// whose curves fall in straight lines (uniform reads over 300 and 900 objects, at rates 4 and 1) and a third of 10
/// objects, so each floor is 200 windows and the tiny one leaves the rest of its own to divide; try every split of the
/// two large ones in whole windows that keeps both floors, by brute force. Expect: the partition's misses a second
/// within 1% of the best split's, and the tiny one given its working set.
#[test]
fn straight_curves_are_divided_as_brute_force_would() {
  let (small, large, tiny) = (
    uniform(300, 30_000, 5),
    uniform(900, 30_000, 6),
    uniform(10, 1_000, 11),
  );
  let wants = [
    Want {
      curve: &small,
      per_s: 4.0,
    },
    Want {
      curve: &large,
      per_s: 1.0,
    },
    Want {
      curve: &tiny,
      per_s: 1.0,
    },
  ];
  let room = 600 * GRANULE;
  let division = partition(room, &wants);
  let (floor, kept) = (room / 3, division.given[2]);
  assert!(kept >= tiny.working_set(), "{division:?}");
  let shared = room - kept;
  let brute = (floor / GRANULE..=(shared - floor) / GRANULE)
    .map(|first| cost(&wants[..2], &[first * GRANULE, shared - first * GRANULE]))
    .fold(f64::INFINITY, f64::min);
  let ours = cost(&wants[..2], &division.given[..2]);
  assert!(
    ours <= brute * 1.01 + 1e-9,
    "{ours} vs {brute}: {division:?}"
  );
}

/// A-846 (the lookahead). Do: divide 1,600 windows among a scan of 900 objects (no saving until all 900 fit), a uniform
/// repository of 2,000 objects whose every window saves a little, and two of 20 objects, at equal rates: floors of 400
/// windows, the small ones using 20 each. Expect: the scan given its whole loop. The steepest-next-byte rule would have
/// fed the uniform one, whose next window saves something where the scan's saves nothing until its last.
#[test]
fn a_scans_step_is_not_starved() {
  let (looping, steady) = (scan(900, 20, 7), uniform(2_000, 18_000, 8));
  let (one, two) = (uniform(20, 1_000, 12), uniform(20, 1_000, 13));
  let wants: Vec<Want<'_>> = [&looping, &steady, &one, &two]
    .into_iter()
    .map(|curve| Want { curve, per_s: 1.0 })
    .collect();
  let division = partition(1_600 * GRANULE, &wants);
  assert!(division.given[0] >= 900 * GRANULE, "{division:?}");
}

/// A-846 ("Work-conserving"). Do: divide 1,000 windows, then 100, between a repository whose objects are each read
/// once (a flat curve: every read a cold miss) and one with a 100-object working set. Expect: the first given nothing,
/// since nothing it could hold saves a miss; the second its working set, at most its last bin edge (127 windows); the
/// rest spare; with 100 windows, the second all of them. With no repositories, all is spare.
#[test]
fn room_no_curve_wants_is_spare() {
  let mut once = Curve::new(1 << 16, GRANULE);
  for object in 0..5_000u64 {
    once.read(key_of(9 << 32 | object), GRANULE);
  }
  let small = uniform(100, 5_000, 10);
  let wants = [
    Want {
      curve: &once,
      per_s: 1.0,
    },
    Want {
      curve: &small,
      per_s: 1.0,
    },
  ];
  let room = 1_000 * GRANULE;
  let division = partition(room, &wants);
  assert_eq!(division.given[0], 0, "{division:?}");
  assert!(
    (100 * GRANULE..=127 * GRANULE).contains(&division.given[1]),
    "{division:?}"
  );
  assert_eq!(division.spare, room - division.given[1]);
  let tight = partition(100 * GRANULE, &wants);
  assert_eq!(tight.given, vec![0, 100 * GRANULE], "{tight:?}");
  let nobody = partition(room, &[]);
  assert_eq!(nobody.given, Vec::<u64>::new());
  assert_eq!(nobody.spare, room);
}
