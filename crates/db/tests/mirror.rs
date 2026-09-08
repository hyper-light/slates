//! Tests for asynchronous mirroring (§4.8 "Mirroring"; D-18; AC-8.x). The home region commits
//! records through the fenced ledger register; the mirror ships the committed prefix to a second
//! region in epoch order, exposing the lag. Mirroring catches up when fully reachable, holds the
//! lag under a majority partition of the mirror's own holders and resumes when it heals, and never
//! commits a record the home did not (it only replays the home's committed prefix).

use std::collections::BTreeSet;

use slates_db::ledger::{Cohort, Owner, Reach};
use slates_db::mirror::Mirror;
use slates_db::register::{HostId, ObjectId, Quorum};

/// A payload identity from a byte.
fn id(byte: u8) -> [u8; 32] {
  [byte; 32]
}

/// A home cohort and its owner at fault tolerance `f`, and a mirror region at the same `f` on a
/// disjoint set of hosts (a separate failure domain).
fn regions(f: u32) -> (Cohort, Owner, Mirror) {
  let home_neighbourhood: Vec<HostId> = (1..=8u64).map(HostId).collect();
  let home = Cohort::new(
    HostId(1),
    &home_neighbourhood,
    ObjectId::new(HostId(1), 42),
    Quorum { f },
  );
  let owner = Owner::bootstrap(&home);
  let mirror_neighbourhood: Vec<HostId> = (101..=108u64).map(HostId).collect();
  let mirror = Mirror::new(
    HostId(101),
    &mirror_neighbourhood,
    ObjectId::new(HostId(1), 42),
    Quorum { f },
  );
  (home, owner, mirror)
}

/// Reach only the mirror candidates at these indices.
fn only(mirror_candidates: &[HostId], indices: &[usize]) -> Reach {
  let set: BTreeSet<HostId> = indices
    .iter()
    .filter_map(|i| mirror_candidates.get(*i).copied())
    .collect();
  Reach::Only(set)
}

/// A fully reachable mirror catches up to the home's committed prefix and the lag falls to zero.
#[test]
fn a_reachable_mirror_catches_up() {
  let (mut home, mut owner, mut mirror) = regions(1);
  for byte in 0..3u8 {
    owner.propose(&mut home, id(byte), &Reach::All);
  }
  let home_committed = home.committed_prefix();
  assert_eq!(mirror.lag(&home_committed), 3, "nothing shipped yet");
  let shipped = mirror.ship(&home_committed, &Reach::All);
  assert_eq!(shipped, 3, "all three records mirror");
  assert_eq!(mirror.lag(&home_committed), 0, "the mirror has caught up");
  assert!(
    mirror.placed_through(2),
    "the last record is placed on the mirror"
  );
}

/// `await placed(mirror)` for a record is false until the mirror commits through its position.
#[test]
fn placed_on_the_mirror_follows_shipping() {
  let (mut home, mut owner, mut mirror) = regions(1);
  owner.propose(&mut home, id(1), &Reach::All);
  owner.propose(&mut home, id(2), &Reach::All);
  let home_committed = home.committed_prefix();
  assert!(!mirror.placed_through(0), "not placed before shipping");
  mirror.ship(&home_committed, &Reach::All);
  assert!(
    mirror.placed_through(0) && mirror.placed_through(1),
    "placed after shipping"
  );
}

/// A minority partition of the mirror's own holders still lets it catch up (a quorum is reachable).
#[test]
fn a_minority_mirror_partition_still_ships() {
  let (mut home, mut owner, mut mirror) = regions(1);
  owner.propose(&mut home, id(9), &Reach::All);
  let home_committed = home.committed_prefix();
  // The mirror has three candidates; reach two of them (a quorum), the third partitioned.
  let two_of_three = only(mirror.candidates(), &[0, 1]);
  let shipped = mirror.ship(&home_committed, &two_of_three);
  assert_eq!(shipped, 1, "two of three commits");
  assert_eq!(mirror.lag(&home_committed), 0);
}

/// A majority partition of the mirror's own holders holds the lag; shipping again after it heals
/// catches the mirror up — the replay is idempotent and resumable.
#[test]
fn a_majority_mirror_partition_holds_the_lag_then_resumes() {
  let (mut home, mut owner, mut mirror) = regions(1);
  for byte in 0..2u8 {
    owner.propose(&mut home, id(byte), &Reach::All);
  }
  let home_committed = home.committed_prefix();
  // Reach only one of the mirror's three holders: below its quorum, nothing commits.
  let one_holder = only(mirror.candidates(), &[0]);
  let shipped = mirror.ship(&home_committed, &one_holder);
  assert_eq!(shipped, 0, "one holder of three does not commit");
  assert_eq!(mirror.lag(&home_committed), 2, "the lag holds");
  // The partition heals: a full ship catches up, replaying the same prefix.
  let shipped = mirror.ship(&home_committed, &Reach::All);
  assert_eq!(shipped, 2, "the mirror catches up after healing");
  assert_eq!(mirror.lag(&home_committed), 0);
}

/// Mirroring is incremental: after the mirror catches up, new home commits ship on the next round
/// and the lag tracks them.
#[test]
fn mirroring_tracks_new_commits() {
  let (mut home, mut owner, mut mirror) = regions(1);
  owner.propose(&mut home, id(1), &Reach::All);
  mirror.ship(&home.committed_prefix(), &Reach::All);
  assert_eq!(mirror.lag(&home.committed_prefix()), 0);
  // The home commits two more.
  owner.propose(&mut home, id(2), &Reach::All);
  owner.propose(&mut home, id(3), &Reach::All);
  let home_committed = home.committed_prefix();
  assert_eq!(mirror.lag(&home_committed), 2, "the new commits are behind");
  let shipped = mirror.ship(&home_committed, &Reach::All);
  assert_eq!(shipped, 2);
  assert_eq!(mirror.lag(&home_committed), 0);
}

/// At f=0 the mirror region is a single holder with a commit of one: it mirrors the home's prefix
/// exactly (the same code as a fleet mirror, R8).
#[test]
fn f0_mirror_is_a_single_holder() {
  let (mut home, mut owner, mut mirror) = regions(0);
  owner.propose(&mut home, id(5), &Reach::All);
  let shipped = mirror.ship(&home.committed_prefix(), &Reach::All);
  assert_eq!(shipped, 1);
  assert_eq!(mirror.committed_len(), 1);
}
