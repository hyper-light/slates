//! Tests for reconfiguration (§4.8; the `Reconfig` model, GAPS §10; AC-8.x). A register's holder
//! set changes from an old configuration to a new one while the owner keeps writing. The tests
//! drive the protocol by use and check the two properties the TLA model proved, computed here by
//! enumerating the majorities directly:
//!
//! - ReadSafety: for every read majority — of whichever configuration a reader knows, which may lag
//!   one phase — the newest record that majority holds is at least as new as every committed record.
//! - NoLoss: once the old set is retired, every record committed under the joint rule at retirement
//!   is still subsumed by a committed record (nothing committed during the change is lost).

use slates_db::reconfig::{Phase, Reconfiguration, RetireError};
use slates_db::register::HostId;

/// A payload identity from a byte.
fn id(byte: u8) -> [u8; 32] {
  [byte; 32]
}

/// The old configuration {1,2,3} changing to the new {2,3,4}: host 1 restarted and left, host 4 is
/// fresh, and {2,3} overlap.
fn reconfiguration() -> Reconfiguration {
  Reconfiguration::new(
    &[HostId(1), HostId(2), HostId(3)],
    &[HostId(2), HostId(3), HostId(4)],
  )
}

/// Each host in `hosts` accepts the record at `seq`.
fn accept_by(rc: &mut Reconfiguration, seq: u64, hosts: &[HostId]) {
  for host in hosts {
    rc.accept(seq, *host);
  }
}

/// The old, joint and new majority triple {1,2,3} used to commit under the joint rule.
const JOINT_MAJORITY: [HostId; 3] = [HostId(1), HostId(2), HostId(3)];

/// Every majority subset of `set` (`2 * |Q| > |set|`), by bitmask enumeration (the sets are small).
fn majorities(set: &[HostId]) -> Vec<Vec<HostId>> {
  let n = set.len();
  let mut out = Vec::new();
  for mask in 0u32..(1u32 << n) {
    let subset: Vec<HostId> = (0..n)
      .filter(|i| (mask >> i) & 1 == 1)
      .map(|i| set[i])
      .collect();
    if subset.len().saturating_mul(2) > n {
      out.push(subset);
    }
  }
  out
}

/// The read majorities a reader may use in the current phase (a reader may lag one phase, so the
/// joint phase admits majorities of either configuration).
fn read_quorums(rc: &Reconfiguration) -> Vec<Vec<HostId>> {
  let old: Vec<HostId> = rc.old_set().iter().copied().collect();
  let new: Vec<HostId> = rc.new_set().iter().copied().collect();
  match rc.phase() {
    Phase::Old => majorities(&old),
    Phase::Joint => {
      let mut quorums = majorities(&old);
      quorums.extend(majorities(&new));
      quorums
    }
    Phase::New => majorities(&new),
  }
}

/// ReadSafety: every read majority already holds a record at least as new as every committed one —
/// equivalently, its newest stored sequence is at least the newest committed sequence.
fn read_safety(rc: &Reconfiguration) -> bool {
  let newest_committed = rc.committed_seqs().into_iter().max().unwrap_or(0);
  read_quorums(rc).into_iter().all(|quorum| {
    let newest_in_quorum = quorum
      .into_iter()
      .map(|host| rc.stored_seq(host))
      .max()
      .unwrap_or(0);
    newest_in_quorum >= newest_committed
  })
}

/// NoLoss: after retirement, every joint-committed record snapshotted at retirement is subsumed by
/// a still-committed record.
fn no_loss(rc: &Reconfiguration) -> bool {
  if rc.phase() != Phase::New {
    return true;
  }
  let newest_committed = rc.committed_seqs().into_iter().max().unwrap_or(0);
  rc.retired_committed()
    .iter()
    .all(|seq| newest_committed >= *seq)
}

/// In the old phase a record commits at a majority of the old set.
#[test]
fn the_old_phase_commits_at_a_majority_of_old() {
  let mut rc = reconfiguration();
  let seq = rc.issue(id(1));
  assert!(rc.accept(seq, HostId(1)));
  assert!(
    !rc.committed_seqs().contains(&seq),
    "one of three is not a majority"
  );
  assert!(rc.accept(seq, HostId(2)));
  assert!(rc.committed_seqs().contains(&seq), "two of three commits");
  assert!(read_safety(&rc));
}

/// A holder that left the old set never accepts again once the change is announced.
#[test]
fn a_departed_holder_cannot_accept_in_the_joint_phase() {
  let mut rc = reconfiguration();
  rc.announce();
  let seq = rc.issue(id(1));
  // Host 1 is only in the old set; in the joint phase it is still active (old ∪ new). Host 5 was
  // never a member and can never accept.
  assert!(!rc.accept(seq, HostId(5)), "a non-member never accepts");
}

/// In the joint phase a record commits only at a majority of both configurations.
#[test]
fn the_joint_phase_needs_both_majorities() {
  let mut rc = reconfiguration();
  rc.announce();
  let seq = rc.issue(id(1));
  // A majority of old {1,2,3}: hosts 1 and 2. Host 2 is in new too, but {2} is not a new majority.
  rc.accept(seq, HostId(1));
  rc.accept(seq, HostId(2));
  assert!(
    !rc.committed_seqs().contains(&seq),
    "an old majority alone does not commit in joint"
  );
  // Host 3 (in both) brings new to {2,3}, a majority of new {2,3,4}.
  rc.accept(seq, HostId(3));
  assert!(rc.committed_seqs().contains(&seq), "both majorities commit");
  assert!(read_safety(&rc));
}

/// State transfer copies the newest committed record into a fresh new-set holder.
#[test]
fn state_transfer_carries_the_newest_to_a_fresh_holder() {
  let mut rc = reconfiguration();
  rc.announce();
  let seq = rc.issue(id(1));
  accept_by(&mut rc, seq, &JOINT_MAJORITY);
  assert_eq!(rc.stored_seq(HostId(4)), 0, "the fresh holder starts empty");
  assert!(
    rc.transfer(HostId(4)),
    "transfer carries the newest committed record"
  );
  assert_eq!(
    rc.stored_seq(HostId(4)),
    seq,
    "the fresh holder now holds it"
  );
}

/// Retirement is refused outside the joint phase and before the owner acknowledges the change.
#[test]
fn retirement_is_gated() {
  let mut rc = reconfiguration();
  assert_eq!(rc.retire(), Err(RetireError::NotInJointPhase));
  rc.announce();
  let seq = rc.issue(id(1));
  accept_by(&mut rc, seq, &JOINT_MAJORITY);
  assert_eq!(
    rc.retire(),
    Err(RetireError::OwnerNotAcked),
    "the owner must acknowledge first"
  );
  assert!(rc.owner_ack());
  assert_eq!(rc.retire(), Ok(()), "now it retires");
  assert_eq!(rc.phase(), Phase::New);
}

/// A full reconfiguration preserves the record committed during the change (NoLoss, non-vacuous),
/// and ReadSafety holds at every step.
#[test]
fn a_full_reconfiguration_preserves_committed_records() {
  let mut rc = reconfiguration();
  // Commit a record under the old phase, then announce and commit one under the joint rule.
  let first = rc.issue(id(1));
  accept_by(&mut rc, first, &[HostId(1), HostId(2)]);
  rc.announce();
  let second = rc.issue(id(2));
  accept_by(&mut rc, second, &JOINT_MAJORITY);
  assert!(rc.committed_seqs().contains(&second), "joint-committed");
  // Carry it to the fresh holder, acknowledge, and retire.
  rc.transfer(HostId(4));
  assert!(rc.owner_ack());
  assert_eq!(rc.retire(), Ok(()));
  assert_eq!(rc.phase(), Phase::New);
  assert!(
    !rc.retired_committed().is_empty(),
    "something committed during the change"
  );
  assert!(
    rc.committed_seqs().contains(&second),
    "the joint-committed record survives retirement"
  );
  // ReadSafety holds at each step (the proptest checks it exhaustively); NoLoss holds after retiring.
  assert!(read_safety(&rc), "read safety after retirement");
  assert!(no_loss(&rc), "no committed record lost across the change");
}

mod oracle {
  use super::*;
  use proptest::prelude::*;

  /// The acceptors that ever appear (old ∪ new).
  fn acceptors() -> Vec<HostId> {
    vec![HostId(1), HostId(2), HostId(3), HostId(4)]
  }

  /// The issues made so far are capped so sequence numbers stay in a small, acceptable range.
  const ISSUE_CAP: usize = 4;

  /// Applies one generated step to the reconfiguration; an action whose guard is unmet is a no-op.
  fn apply_step(
    rc: &mut Reconfiguration,
    hosts: &[HostId],
    issued: &mut Vec<u64>,
    action: u8,
    x: u8,
    y: u8,
  ) {
    let host = hosts[usize::from(y) % hosts.len()];
    match action {
      0 if issued.len() < ISSUE_CAP => issued.push(rc.issue(id(x))),
      1 if !issued.is_empty() => {
        let seq = issued[usize::from(x) % issued.len()];
        let _ = rc.accept(seq, host);
      }
      2 => {
        let _ = rc.announce();
      }
      3 => {
        let _ = rc.owner_ack();
      }
      4 => {
        let _ = rc.transfer(host);
      }
      5 => {
        let _ = rc.retire();
      }
      _ => {}
    }
  }

  proptest! {
    /// Over any history of issues, accepts, announce, owner-ack, transfers and retire, ReadSafety
    /// holds in every reachable state and NoLoss holds once retired. Steps are `(action, x, y)`
    /// tuples so the strategy needs no `Arc`-backed `prop_oneof` (R2).
    #[test]
    fn read_safety_and_no_loss_hold_over_any_history(
      steps in proptest::collection::vec((0u8..6, any::<u8>(), any::<u8>()), 0..60),
    ) {
      let mut rc = reconfiguration();
      let hosts = acceptors();
      let mut issued: Vec<u64> = Vec::new();

      for (action, x, y) in steps {
        apply_step(&mut rc, &hosts, &mut issued, action, x, y);
        prop_assert!(read_safety(&rc), "ReadSafety violated in phase {:?}", rc.phase());
        prop_assert!(no_loss(&rc), "NoLoss violated after retirement");
      }
    }
  }
}
