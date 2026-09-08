//! Phase-one promotion oracle (§4.8 "Promotion and takeover"): over arbitrary histories of commits and
//! takeovers, the transport-driven register — `Acceptor`, `commit_over_holders`, `promote_over_holders`
//! from [`slates_db::register`] — preserves its safety properties on every generated history (the
//! model-based/oracle discipline the project institutionalizes; proptest with shrinking). The `ledger.rs`
//! simulation proves the multi-position algorithm; this proves the single-value register the cluster
//! plane actually ships over the wire. The properties, each checked against a serial reference:
//!
//! - **Agreement / no-rewrite**: a value committed at a ledger position is never replaced by a different
//!   one — not by a later write, and not by a takeover re-committing an adopted head.
//! - **Continuity**: a takeover whose promotion reaches a quorum adopts a head at least as new as the
//!   last committed one (a promotion quorum and every commit quorum are both `f + 1` of `2f + 1`, so they
//!   intersect and the committed head is in the promises), and it never rewrites the committed value.
//! - **StaleNeverCommits**: after a takeover, the superseded owner — its authority moved on — reaches no
//!   quorum, so it never commits again.
//! - **Non-vacuity**: a full-reach commit commits, so a silently dead protocol cannot pass the oracle.
//!
//! Test by use (R5): the register is driven only through its public commit and promote API.

// Test harness: an unwrap or expect here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use proptest::prelude::*;
use slates_db::register::{
  Acceptor, Authority, Holder, HostEpoch, HostId, Prepare, Promoter, Quorum, Record,
  commit_over_holders, promote_over_holders, rendezvous_first,
};

/// The single object every history writes (one register).
const OBJECT: u64 = 0;

/// The fleet under test and its serial reference: one real [`Acceptor`] per candidate, plus the
/// bookkeeping the oracle checks — the current owner/epoch/generation, the next sequence the owner
/// will write, and the committed-head history (a position's committed value, which must never change).
struct Fleet {
  candidates: Vec<HostId>,
  holders: BTreeMap<HostId, Acceptor>,
  quorum: Quorum,
  owner: HostId,
  epoch: HostEpoch,
  generation: u64,
  next_seq: u64,
  history: BTreeMap<u64, Vec<u8>>,
  committed: Option<(u64, Vec<u8>)>,
  last_adopted: bool,
}

impl Fleet {
  /// A fresh fleet at fault tolerance `f`: `2f + 1` candidate holders, the first the initial owner,
  /// all under generation 0 and epoch 1, nothing committed.
  fn new(f: u32) -> Fleet {
    let count = 2 * u64::from(f) + 1;
    let candidates: Vec<HostId> = (1..=count).map(HostId).collect();
    let owner = candidates[0];
    let authority = Authority {
      generation: 0,
      owner,
    };
    let holders = candidates
      .iter()
      .map(|&id| (id, Acceptor::new(id, authority)))
      .collect();
    Fleet {
      candidates,
      holders,
      quorum: Quorum { f },
      owner,
      epoch: HostEpoch(1),
      generation: 0,
      next_seq: 0,
      history: BTreeMap::new(),
      committed: None,
      last_adopted: false,
    }
  }

  /// The candidate holders reachable under `mask` (bit `i` selects candidate `i`), always including
  /// `required` — the owner reaches its own local hold on a commit, the new owner on a promotion.
  fn reachable(&self, mask: u8, required: HostId) -> Vec<HostId> {
    let mut ids: Vec<HostId> = self
      .candidates
      .iter()
      .copied()
      .enumerate()
      .filter(|(index, host)| *host == required || (mask >> (index % 8)) & 1 == 1)
      .map(|(_, host)| host)
      .collect();
    if !ids.contains(&required) {
      ids.push(required);
    }
    ids
  }

  /// Mutable references to the holders whose id is in `ids`, as `&mut dyn Holder` for a commit round.
  fn holder_refs(&mut self, ids: &[HostId]) -> Vec<&mut dyn Holder> {
    self
      .holders
      .iter_mut()
      .filter(|(id, _)| ids.contains(id))
      .map(|(_, acceptor)| acceptor as &mut dyn Holder)
      .collect()
  }

  /// The owner commits `value` at the next sequence to the holders reachable under `mask` (itself
  /// always among them). On a quorum the head is committed: recorded in the history and as the newest
  /// committed head. The sequence advances whether or not the write placed (the owner never reuses a
  /// position), so a partial write leaves its position used but uncommitted.
  fn commit(&mut self, value: Vec<u8>, mask: u8) {
    let seq = self.next_seq;
    let record = Record {
      owner: self.owner,
      object: OBJECT,
      sequence: seq,
      epoch: self.epoch,
      generation: self.generation,
      value: value.clone(),
    };
    let ids = self.reachable(mask, self.owner);
    let candidates = self.candidates.clone();
    let mut refs = self.holder_refs(&ids);
    let placement = commit_over_holders(&candidates, &record, &mut refs);
    if placement.placed(self.quorum) {
      self.history.insert(seq, value.clone());
      self.committed = Some((seq, value));
    }
    self.next_seq += 1;
  }

  /// A full-reach commit — reaches every candidate, so it always places (non-vacuity anchor).
  fn commit_full(&mut self, value: Vec<u8>) {
    self.commit(value, u8::MAX);
  }

  /// Whether the last takeover reached a promotion quorum and adopted a head — the non-vacuity signal
  /// the guard test asserts moved, so a takeover path that silently stopped reaching quorum (making the
  /// Continuity and StaleNeverCommits checks vacuous) cannot masquerade as a passing oracle.
  fn last_takeover_adopted(&self) -> bool {
    self.last_adopted
  }

  /// A takeover: the configuration assigns the object to the rendezvous-first survivor, bumps the epoch
  /// and generation, and distributes the new authority to every holder (fencing the old owner by
  /// generation). The new owner runs phase one over the holders reachable under `mask` (itself always
  /// among them); if it reaches a promotion quorum, Continuity is asserted and the adopted head is
  /// re-committed under the new epoch. StaleNeverCommits is asserted after every takeover. At `f = 0`
  /// there is no survivor, so the takeover is a no-op.
  fn takeover(&mut self, mask: u8) -> Result<(), TestCaseError> {
    self.last_adopted = false;
    let survivors: Vec<HostId> = self
      .candidates
      .iter()
      .copied()
      .filter(|host| *host != self.owner)
      .collect();
    let Some(new_owner) = rendezvous_first(&survivors, OBJECT) else {
      return Ok(());
    };
    let prev_owner = self.owner;
    let prev_epoch = self.epoch;
    let prev_generation = self.generation;

    self.epoch = HostEpoch(self.epoch.0 + 1);
    self.generation += 1;
    let new_authority = Authority {
      generation: self.generation,
      owner: new_owner,
    };
    for holder in self.holders.values_mut() {
      holder.install_authority(new_authority).unwrap();
    }

    let prepare = Prepare {
      owner: new_owner,
      object: OBJECT,
      epoch: self.epoch,
      generation: self.generation,
    };
    let ids = self.reachable(mask, new_owner);
    let candidates = self.candidates.clone();
    let promotion = {
      let mut refs: Vec<&mut dyn Promoter> = self
        .holders
        .iter_mut()
        .filter(|(id, _)| ids.contains(id))
        .map(|(_, acceptor)| acceptor as &mut dyn Promoter)
        .collect();
      promote_over_holders(&candidates, &prepare, &mut refs)
    };

    if promotion.promoted(self.quorum) {
      if let Some((committed_seq, committed_value)) = self.committed.clone() {
        let adopted = promotion.adopted.clone().ok_or_else(|| {
          TestCaseError::fail("a promotion quorum after a commit must adopt a head")
        })?;
        prop_assert!(
          adopted.sequence >= committed_seq,
          "Continuity: adopted position {} is behind the committed {}",
          adopted.sequence,
          committed_seq
        );
        if adopted.sequence == committed_seq {
          prop_assert_eq!(
            adopted.value,
            committed_value,
            "the committed head must not be rewritten by adoption"
          );
        }
      }

      if let Some(adoption) = promotion.adoption_record(&prepare) {
        let seq = adoption.sequence;
        let value = adoption.value.clone();
        let mut refs = self.holder_refs(&ids);
        let placement = commit_over_holders(&candidates, &adoption, &mut refs);
        if placement.placed(self.quorum) {
          if let Some(existing) = self.history.get(&seq) {
            prop_assert_eq!(
              existing,
              &value,
              "the re-committed head must not rewrite a committed value at position {}",
              seq
            );
          }
          self.history.insert(seq, value.clone());
          self.committed = Some((seq, value));
        }
        self.next_seq = self.next_seq.max(seq + 1);
        self.last_adopted = true;
      }
    }
    // The configuration assigned the object to the new owner regardless of whether phase one confirmed;
    // an unconfirmed new owner retries. Either way the old owner is fenced.
    self.owner = new_owner;

    // StaleNeverCommits: the superseded owner, at its old epoch and generation, reaches no quorum.
    let stale = Record {
      owner: prev_owner,
      object: OBJECT,
      sequence: self.next_seq,
      epoch: prev_epoch,
      generation: prev_generation,
      value: b"stale".to_vec(),
    };
    let placement = {
      let mut refs: Vec<&mut dyn Holder> = self
        .holders
        .values_mut()
        .map(|acceptor| acceptor as &mut dyn Holder)
        .collect();
      commit_over_holders(&candidates, &stale, &mut refs)
    };
    prop_assert!(
      !placement.placed(self.quorum),
      "StaleNeverCommits: the superseded owner committed after a takeover"
    );
    Ok(())
  }
}

proptest! {
  // Takeover/commit interleavings that expose an epoch or fence bug are a sparse needle; a high case
  // count exercises the class in well under a second (the example tests in register.rs are the
  // deterministic guards).
  #![proptest_config(ProptestConfig { cases: 8192, ..ProptestConfig::default() })]

  /// Over any history of commits and takeovers at f in {0, 1, 2}, the register preserves Agreement /
  /// no-rewrite, Continuity, StaleNeverCommits and non-vacuity. Each step is `(is_takeover, value, mask)`
  /// generated as a plain tuple, so the strategy needs no `prop_oneof` (which is `Arc`-backed, R2).
  #[test]
  fn arbitrary_commit_and_takeover_histories_preserve_safety(
    f in 0u32..=2,
    steps in proptest::collection::vec((any::<bool>(), any::<u8>(), any::<u8>()), 0..32),
  ) {
    let mut fleet = Fleet::new(f);
    // Non-vacuity: a leading full-reach commit commits, so a silently dead protocol cannot pass.
    fleet.commit_full(b"lead".to_vec());
    prop_assert_eq!(
      fleet.committed.as_ref().map(|(seq, _)| *seq),
      Some(0),
      "the leading full-reach commit must commit"
    );

    for (is_takeover, value_byte, mask) in steps {
      if is_takeover {
        fleet.takeover(mask)?;
      } else {
        fleet.commit(vec![value_byte], mask);
      }
    }
  }
}

/// Non-vacuity guard for the proptest oracle above (§4.8; the project's institutionalized rule that a
/// silently-dead path can never masquerade as a passing oracle). If the `Fleet` harness ever stopped
/// reaching a promotion quorum — a broken `reachable`, a fence bug that refused every prepare — the
/// oracle's Continuity and StaleNeverCommits checks would pass vacuously. This drives one commit and one
/// takeover through the same harness at `f = 1` and asserts the observable effects the oracle relies on
/// actually happen: the promotion adopts the committed head, and the superseded owner is then refused.
#[test]
fn the_takeover_path_is_non_vacuous() {
  let mut fleet = Fleet::new(1);
  fleet.commit_full(b"head".to_vec());
  assert_eq!(
    fleet.committed.as_ref().map(|(seq, _)| *seq),
    Some(0),
    "the commit committed"
  );

  // A full-reach takeover must reach a promotion quorum and adopt the committed head — the safety
  // branches the proptest depends on actually execute (and its StaleNeverCommits check ran, since
  // takeover returning Ok means the superseded owner was proven unable to commit).
  fleet
    .takeover(u8::MAX)
    .expect("the takeover preserved the safety properties");
  assert!(
    fleet.last_takeover_adopted(),
    "the takeover reached a promotion quorum and adopted a head — the oracle is not vacuous"
  );
  assert_eq!(
    fleet.committed.as_ref().map(|(_, value)| value.clone()),
    Some(b"head".to_vec()),
    "the committed head survived the takeover under the new epoch (Continuity)"
  );
}
