//! Tests for the fenced ledger register (§4.8 "Promotion and takeover"; §4.16 "Commit"; AC-8.1,
//! T-8.15). The register is driven by use: an owner proposes records to its cohort, a minority
//! partition still commits, a majority partition does not, a takeover fences the old owner and
//! adopts the committed prefix, and a superseded owner can never commit again. The proptest oracle
//! drives arbitrary histories of proposals, partitions and takeovers and asserts Agreement (no
//! position holds two quorum-agreed identities), NoLoss and TotalOrder (the committed prefix only
//! grows and never rewrites a committed value), and Continuity (a takeover adopts a log that begins
//! with the committed prefix).

use std::collections::BTreeSet;

use slates_db::ledger::{Cohort, Owner, Reach, TakeoverError};
use slates_db::register::{HostId, Quorum};

/// A payload identity from a single byte (its 32 repeated); distinct bytes give distinct records.
fn id(byte: u8) -> [u8; 32] {
  [byte; 32]
}

/// A fresh cohort and its first owner at fault tolerance `f`. The neighbourhood is large enough for
/// `2f + 1` candidates; the actual candidate ids come from rendezvous, so tests read them from the
/// cohort rather than assuming them.
fn fleet(f: u32) -> (Cohort, Owner) {
  let neighbourhood: Vec<HostId> = (1..=8u64).map(HostId).collect();
  let cohort = Cohort::new(HostId(1), &neighbourhood, 42, Quorum { f });
  let owner = Owner::bootstrap(&cohort);
  (cohort, owner)
}

/// Reach only the candidates at these indices into the cohort's candidate list.
fn only(cohort: &Cohort, indices: &[usize]) -> Reach {
  let candidates = cohort.candidates();
  let set: BTreeSet<HostId> = indices
    .iter()
    .filter_map(|i| candidates.get(*i).copied())
    .collect();
  Reach::Only(set)
}

/// Reach every candidate but the one at `index`.
fn all_but(cohort: &Cohort, index: usize) -> Reach {
  let set: BTreeSet<HostId> = cohort
    .candidates()
    .iter()
    .enumerate()
    .filter(|(i, _)| *i != index)
    .map(|(_, host)| *host)
    .collect();
  Reach::Only(set)
}

/// T-8.15: three proposals to a full cohort all commit and read back as the committed prefix.
#[test]
fn proposals_commit_and_read_back() {
  let (mut cohort, mut owner) = fleet(1);
  for byte in 0..3u8 {
    let commit = owner.propose(&mut cohort, id(byte), &Reach::All);
    assert!(commit.committed, "byte {byte} commits with a full cohort");
    assert_eq!(commit.acked.len(), 3, "all three candidates acknowledge");
  }
  assert_eq!(cohort.committed_prefix(), vec![id(0), id(1), id(2)]);
}

/// AC-8.1 (the register slice at f=0): the laptop is the local append — one candidate, a commit of
/// one — the same code as the fleet, differing only by the quorum's own count.
#[test]
fn f0_is_the_local_append() {
  let (mut cohort, mut owner) = fleet(0);
  assert_eq!(cohort.candidates().len(), 1, "f=0: the owner alone");
  let commit = owner.propose(&mut cohort, id(7), &Reach::All);
  assert!(commit.committed, "f=0 commits at one acknowledgement");
  assert_eq!(cohort.committed_prefix(), vec![id(7)]);
}

/// A minority partition (one holder unreachable of three) still commits: two acknowledgements reach
/// the commit quorum.
#[test]
fn a_minority_partition_still_commits() {
  let (mut cohort, mut owner) = fleet(1);
  let reach = all_but(&cohort, 2);
  let commit = owner.propose(&mut cohort, id(1), &reach);
  assert!(commit.committed, "two of three commits");
  assert_eq!(commit.acked.len(), 2);
  assert_eq!(cohort.committed_prefix(), vec![id(1)]);
}

/// A majority partition (only one holder reachable of three) does not commit: one acknowledgement
/// is below the commit quorum, and nothing is committed.
#[test]
fn a_majority_partition_does_not_commit() {
  let (mut cohort, mut owner) = fleet(1);
  let reach = only(&cohort, &[0]);
  let commit = owner.propose(&mut cohort, id(1), &reach);
  assert!(!commit.committed, "one of three does not commit");
  assert!(cohort.committed_prefix().is_empty(), "nothing committed");
}

/// A takeover is refused when fewer than a quorum of candidates are reachable, rather than splitting
/// the register.
#[test]
fn a_takeover_needs_a_quorum() {
  let (mut cohort, _owner) = fleet(1);
  let reach = only(&cohort, &[1]);
  let outcome = Owner::take_over(&mut cohort, HostId(2), &reach);
  assert!(
    matches!(
      outcome,
      Err(TakeoverError::NoQuorum {
        reachable: 1,
        needed: 2
      })
    ),
    "a sub-quorum takeover is refused"
  );
}

/// A takeover fences the old owner and adopts the committed prefix; the new owner continues the log
/// and its epoch is above the old one.
#[test]
fn a_takeover_adopts_the_committed_prefix() {
  let (mut cohort, mut owner) = fleet(1);
  owner.propose(&mut cohort, id(10), &Reach::All);
  owner.propose(&mut cohort, id(11), &Reach::All);
  let before = cohort.committed_prefix();
  let successor = cohort.candidates()[1];
  let new_owner = Owner::take_over(&mut cohort, successor, &Reach::All).expect("quorum");
  assert!(new_owner.epoch > owner.epoch, "the epoch is bumped");
  assert_eq!(new_owner.len(), 2, "the two committed records are adopted");
  // Continuity: the adopted log begins with everything that had committed.
  assert_eq!(
    cohort.committed_prefix(),
    before,
    "no committed record is lost"
  );
}

/// StaleNeverCommits: after a takeover fences a quorum, the superseded owner's proposal reaches at
/// most `f` holders and never commits, while the new owner commits normally, and the register never
/// diverges.
#[test]
fn a_superseded_owner_never_commits() {
  let (mut cohort, mut old_owner) = fleet(1);
  let first = old_owner.propose(&mut cohort, id(1), &Reach::All);
  assert!(first.committed);
  let successor = cohort.candidates()[1];
  let mut new_owner = Owner::take_over(&mut cohort, successor, &Reach::All).expect("quorum");
  // The old owner, now fenced, tries to append: it cannot commit.
  let stale = old_owner.propose(&mut cohort, id(2), &Reach::All);
  assert!(!stale.committed, "the fenced owner never commits");
  // The new owner appends normally.
  let fresh = new_owner.propose(&mut cohort, id(3), &Reach::All);
  assert!(fresh.committed, "the new owner commits");
  assert_eq!(cohort.committed_prefix(), vec![id(1), id(3)]);
  assert!(
    !cohort.diverges_at(0) && !cohort.diverges_at(1),
    "no position diverges"
  );
}

/// Continuity across a partial write: a record acknowledged by only the owner (below the quorum)
/// before a takeover is not committed; the takeover's phase-one read from a quorum that lacks it
/// safely drops it, and every committed record is preserved.
#[test]
fn a_partial_write_is_dropped_and_committed_records_survive() {
  let (mut cohort, mut owner) = fleet(1);
  owner.propose(&mut cohort, id(1), &Reach::All);
  owner.propose(&mut cohort, id(2), &Reach::All);
  // A third reaches only the owner (candidate 0): one acknowledgement, not committed.
  let owner_only = only(&cohort, &[0]);
  let partial = owner.propose(&mut cohort, id(3), &owner_only);
  assert!(!partial.committed);
  // Takeover reads a quorum that excludes the owner, so it never saw the partial record.
  let successor = cohort.candidates()[1];
  let quorum_without_owner = all_but(&cohort, 0);
  let new_owner = Owner::take_over(&mut cohort, successor, &quorum_without_owner).expect("quorum");
  assert_eq!(
    new_owner.len(),
    2,
    "the uncommitted third record is dropped"
  );
  assert_eq!(
    cohort.committed_prefix(),
    vec![id(1), id(2)],
    "committed records survive"
  );
}

/// The observable outcome of a single full-cohort proposal is the same at f=0 and f=1 — both
/// commit — because it is one body of code with a different quorum count (R8, the N=1 differential).
#[test]
fn the_commit_outcome_is_the_same_at_f0_and_f1() {
  let (mut laptop, mut laptop_owner) = fleet(0);
  let (mut fleet_cohort, mut fleet_owner) = fleet(1);
  let on_laptop = laptop_owner.propose(&mut laptop, id(5), &Reach::All);
  let on_fleet = fleet_owner.propose(&mut fleet_cohort, id(5), &Reach::All);
  assert_eq!(on_laptop.committed, on_fleet.committed, "both commit");
  assert_eq!(laptop.committed_prefix(), fleet_cohort.committed_prefix());
}

mod oracle {
  use super::*;
  use proptest::prelude::*;

  /// Whether candidate `index` is reachable under `mask`: the owner (index 0) always is, so a
  /// proposal reaches its proposer; every other candidate is reachable when its bit is set. The
  /// index is a small candidate position (at most `2f`), so the shift never overflows the mask.
  fn reachable(mask: u8, index: usize) -> bool {
    if index == 0 {
      return true;
    }
    match u32::try_from(index)
      .ok()
      .and_then(|shift| 1u8.checked_shl(shift))
    {
      Some(bit) => mask & bit != 0,
      None => false,
    }
  }

  /// A reach from a bitmask over the cohort's candidate positions.
  fn reach_of(cohort: &Cohort, mask: u8) -> Reach {
    let set: BTreeSet<HostId> = cohort
      .candidates()
      .iter()
      .enumerate()
      .filter(|(i, _)| reachable(mask, *i))
      .map(|(_, host)| *host)
      .collect();
    Reach::Only(set)
  }

  proptest! {
    /// Over any history at f in {0,1,2}, the register never violates its safety properties:
    /// Agreement (no position holds two quorum-agreed identities), NoLoss and TotalOrder (the
    /// committed prefix only extends and never rewrites a committed value), Continuity (a takeover
    /// adopts at least the committed prefix), and non-vacuity (the leading full-cohort proposal
    /// always commits, so a dead protocol cannot pass). Each step is `(is_takeover, byte, mask)`,
    /// generated as plain tuples so the strategy needs no `prop_oneof` (which is `Arc`-backed, R2).
    #[test]
    fn arbitrary_histories_preserve_safety(
      f in 0u32..=2,
      steps in proptest::collection::vec((any::<bool>(), any::<u8>(), any::<u8>()), 0..40),
    ) {
      let (mut cohort, mut owner) = fleet(f);
      // A guaranteed leading commit gives non-vacuity: the prefix is non-empty from here on.
      let lead = owner.propose(&mut cohort, id(200), &Reach::All);
      prop_assert!(lead.committed, "the leading full-cohort proposal commits");
      let mut committed = cohort.committed_prefix();
      prop_assert_eq!(committed.clone(), vec![id(200)]);

      for (is_takeover, byte, mask) in steps {
        let reach = reach_of(&cohort, mask);
        if is_takeover {
          if let Ok(new_owner) = Owner::take_over(&mut cohort, HostId(9), &reach) {
            // Continuity: the adopted log begins with everything committed at takeover.
            let prefix = cohort.committed_prefix();
            prop_assert!(new_owner.len() >= prefix.len(), "adopts at least the committed prefix");
            owner = new_owner;
          }
        } else {
          let _ = owner.propose(&mut cohort, id(byte), &reach);
        }

        // Agreement: no position is claimed by two different quorums.
        let prefix = cohort.committed_prefix();
        for position in 0..prefix.len() {
          prop_assert!(!cohort.diverges_at(position), "position {} diverges", position);
        }
        // NoLoss + TotalOrder: the previous committed prefix is still a prefix.
        prop_assert!(
          prefix.starts_with(&committed),
          "committed prefix shrank or was rewritten: was {:?}, now {:?}",
          committed,
          prefix
        );
        committed = prefix;
      }
    }
  }
}
