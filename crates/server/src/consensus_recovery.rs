//! Explicit quorum-loss recovery (§4.8, AUD-07). A human capability authorizes one reviewed
//! retained copy after the former group is fenced. Recovery creates a distinct consensus identity;
//! neither discovery nor a timeout may authorize it (Raft §5.2; etcd disaster recovery).

use slates_ipc::protocol::{ConsensusRecoveryPlan, Refusal, ReplyBody};

use crate::state::ShardState;

/// A node-specific operator capability for quorum-loss recovery (§4.8). It authorizes
/// no landing or consumer enrollment. Debug output always hides the key.
#[derive(Clone)]
pub struct RecoveryKey([u8; 32]);

impl std::fmt::Debug for RecoveryKey {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str("RecoveryKey([redacted])")
  }
}

impl RecoveryKey {
  /// Accepts a provisioned 256-bit key; the all-zero unprovisioned value refuses.
  pub fn from_bytes(bytes: [u8; 32]) -> Option<Self> {
    (bytes != [0; 32]).then_some(Self(bytes))
  }

  /// Authorizes the exact reviewed plan without exposing the key bytes.
  pub fn proof(&self, plan: &[u8; 32]) -> [u8; 32] {
    recovery_proof(&self.0, plan)
  }
}

fn secret(state: &ShardState) -> &[u8; 32] {
  state
    .config
    .recovery_key
    .as_ref()
    .map_or(&state.issuer_secret, |key| &key.0)
}

/// Proves the human surface reviewed this exact plan (§4.13). The plan binds the group,
/// retained state, destination and daemon start, so the proof cannot authorize another reset.
pub fn recovery_proof(secret: &[u8; 32], plan: &[u8; 32]) -> [u8; 32] {
  let mut hash = blake3::Hasher::new_keyed(secret);
  hash.update(b"slates/consensus-recovery/v1");
  hash.update(plan);
  *hash.finalize().as_bytes()
}

/// A join is authority to replace one specific old group with one specific new group.
/// The old Raft state stays intact until the replacement fetch has validated completely.
#[derive(Clone, slates_wire::Wire)]
pub(crate) struct JoinTarget {
  pub(crate) group: [u8; 32],
  pub(crate) floor: u64,
}

#[derive(Clone, slates_wire::Wire)]
struct Receipt {
  digest: [u8; 32],
  group: [u8; 32],
  joining: bool,
}

/// Bounded operator state: one pending join and one completed authorization per group.
#[derive(Clone, Default, slates_wire::Wire)]
pub(crate) struct RecoveryState {
  pub(crate) council: Option<JoinTarget>,
  pub(crate) root: Option<JoinTarget>,
  council_receipt: Option<Receipt>,
  root_receipt: Option<Receipt>,
}

impl RecoveryState {
  pub(crate) fn joining(&self) -> bool {
    self.council.is_some() || self.root.is_some()
  }

  pub(crate) fn target(&self, root: bool) -> Option<&JoinTarget> {
    if root {
      self.root.as_ref()
    } else {
      self.council.as_ref()
    }
  }
}

pub(crate) fn plan(
  state: &ShardState,
  root: bool,
  target: Option<[u8; 32]>,
) -> Result<ConsensusRecoveryPlan, Refusal> {
  use slates_wire::Wire;
  let unavailable = Refusal::ConsensusRecoveryUnavailable;
  let (raft, base, view, previous, version, voters) = if root {
    let (raft, base) = state.root.join_state().ok_or(unavailable.clone())?;
    (
      raft,
      slates_cluster::raft_wire::encode_root_configuration(&base),
      slates_cluster::raft_wire::encode_root_configuration(state.root.configuration()),
      state.root_group,
      state.root.configuration().version,
      state.root.voters(),
    )
  } else {
    let (raft, base) = state.council.join_state().ok_or(unavailable.clone())?;
    (
      raft,
      slates_cluster::raft_wire::encode_regional_configuration(&base),
      slates_cluster::raft_wire::encode_regional_configuration(state.council.configuration()),
      state.council_group,
      state.council.configuration().version,
      state.council.voters(),
    )
  };
  let previous = previous.ok_or(unavailable.clone())?;
  if target == Some(previous) {
    return Err(Refusal::ConsensusRecoveryStale);
  }
  let mut hash = blake3::Hasher::new_keyed(secret(state));
  hash.update(b"slates/consensus-recovery-plan/v1");
  hash.update(&[u8::from(root)]);
  hash.update(&previous);
  hash.update(&target.to_bytes());
  hash.update(&state.recovery.target(root).cloned().to_bytes());
  hash.update(&raft.to_bytes());
  hash.update(&base);
  hash.update(&view);
  Ok(ConsensusRecoveryPlan {
    member: state.fleet.host().0,
    previous,
    digest: *hash.finalize().as_bytes(),
    committed: raft.commit_index,
    last_log: raft
      .snapshot_index
      .checked_add(u64::try_from(raft.log.len()).map_err(|_| unavailable.clone())?)
      .ok_or(unavailable)?,
    version,
    voters: voters.into_iter().map(|host| host.0).collect(),
    target,
  })
}

pub(crate) fn recover(
  state: &mut ShardState,
  root: bool,
  target: Option<[u8; 32]>,
  digest: [u8; 32],
  proof: [u8; 32],
) -> ReplyBody {
  let expected = recovery_proof(secret(state), &digest);
  if !crate::landing::constant_time_eq(&expected, &proof) {
    return ReplyBody::Refused {
      refusal: Refusal::GrantIssuerUnverified,
    };
  }
  let receipt = if root {
    &state.recovery.root_receipt
  } else {
    &state.recovery.council_receipt
  };
  if let Some(receipt) = receipt
    && receipt.digest == digest
  {
    return ReplyBody::RecoveryStarted {
      group: receipt.group,
      joining: receipt.joining,
    };
  }
  let reviewed = match plan(state, root, target) {
    Ok(reviewed) if reviewed.digest == digest => reviewed,
    Ok(_) => {
      return ReplyBody::Refused {
        refusal: Refusal::ConsensusRecoveryStale,
      };
    }
    Err(refusal) => return ReplyBody::Refused { refusal },
  };
  let group = match target {
    Some(group) => {
      let pending = Some(JoinTarget {
        group,
        floor: reviewed.version,
      });
      if root {
        state.root.suspend();
        state.recovery.root = pending;
      } else {
        state.council.suspend();
        state.recovery.council = pending;
      }
      group
    }
    None => match reform(state, root) {
      Ok(group) => {
        if root {
          state.recovery.root = None;
        } else {
          state.recovery.council = None;
        }
        group
      }
      Err(refusal) => return ReplyBody::Refused { refusal },
    },
  };
  let receipt = Some(Receipt {
    digest,
    group,
    joining: target.is_some(),
  });
  if root {
    state.recovery.root_receipt = receipt;
  } else {
    state.recovery.council_receipt = receipt;
  }
  state.bootstrap_authorized = None;
  state.consensus_ready = false;
  // A pending join changes authority without changing the old Raft log. Publish it explicitly
  // before any acknowledgement; ordinary Raft mutations use the shard's transition barrier.
  if crate::retention::publish_authorization(state).is_err() {
    return ReplyBody::Refused {
      refusal: Refusal::ConsensusRecoveryUnavailable,
    };
  }
  if !state.recovery.joining() {
    install_recovered_authority(state);
  }
  ReplyBody::RecoveryStarted {
    group,
    joining: target.is_some(),
  }
}

fn reform(state: &mut ShardState, root: bool) -> Result<[u8; 32], Refusal> {
  use slates_cluster::raft_wire::{encode_regional_configuration, encode_root_configuration};
  use slates_cluster::{config_group::RegionalCouncil, root_group::RootGroup};
  let unavailable = Refusal::ConsensusRecoveryUnavailable;
  let local = state.fleet.host();
  if root {
    let mut base = state.root.configuration().clone();
    base.version = base.version.checked_add(1).ok_or(unavailable.clone())?;
    let group = RootGroup::reform(local, base);
    let (raft, base) = group.join_state().ok_or(unavailable)?;
    let identity = crate::consensus::genesis(true, &raft, &encode_root_configuration(&base));
    state.root = group;
    state.root_group = Some(identity);
    Ok(identity)
  } else {
    let mut base = state.council.configuration().clone();
    base.version = base.version.checked_add(1).ok_or(unavailable.clone())?;
    for epoch in base.epochs.values_mut() {
      epoch.0 = epoch.0.checked_add(1).ok_or(unavailable.clone())?;
    }
    for neighbourhood in base.neighbourhoods.values_mut() {
      neighbourhood.generation = base.version;
    }
    let group = RegionalCouncil::reform(local, base, state.council.scatter());
    let (raft, base) = group.join_state().ok_or(unavailable)?;
    let identity = crate::consensus::genesis(false, &raft, &encode_regional_configuration(&base));
    state.council = group;
    state.council_group = Some(identity);
    Ok(identity)
  }
}

fn install_recovered_authority(state: &mut ShardState) {
  let local = state.fleet.host();
  if let Some(configuration) = state.council.configuration().configuration_for(local) {
    state.durability_shortfall = state
      .config
      .fleet
      .as_ref()
      .and_then(|fleet| fleet.durability)
      .and_then(|bound| bound.shortfall(&configuration));
    state
      .fleet
      .install_configuration(configuration, &state.council.configuration().members);
  }
  state.consensus_ready = state.council.initialized()
    && state.root.initialized()
    && state.council.configuration().members.contains(&local);
}

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used, clippy::panic)]
  use super::*;
  use slates_cluster::root_group::RootCommand;
  use slates_db::register::{ObjectId, RegionId};

  fn move_home(state: &mut ShardState, object: ObjectId) {
    assert!(state.root.propose(RootCommand::AdmitRegion(RegionId(2))));
    assert!(state.root.propose(RootCommand::MoveHome {
      volume: object,
      to: RegionId(2)
    }));
  }

  /// AC-8.1, §4.8: recover a reviewed root copy after a quorum loss; its moved homes must
  /// survive, old-group messages must be refused, and a changed plan must not authorize a reset.
  #[test]
  fn explicit_recovery_preserves_moved_homes_and_refuses_stale_approval() {
    crate::daemon::audit_on_shard(|state| {
      let object = ObjectId([1; 16]);
      move_home(state, object);
      let reviewed = plan(state, true, None).unwrap();
      let proof = recovery_proof(&state.issuer_secret, &reviewed.digest);
      assert!(matches!(
        recover(state, true, None, reviewed.digest, [0; 32]),
        ReplyBody::Refused {
          refusal: Refusal::GrantIssuerUnverified
        }
      ));
      assert!(state.root.propose(RootCommand::AdmitRegion(RegionId(3))));
      assert!(matches!(
        recover(state, true, None, reviewed.digest, proof),
        ReplyBody::Refused {
          refusal: Refusal::ConsensusRecoveryStale
        }
      ));
      let reviewed = plan(state, true, None).unwrap();
      let proof = recovery_proof(&state.issuer_secret, &reviewed.digest);
      let reply = recover(state, true, None, reviewed.digest, proof);
      let ReplyBody::RecoveryStarted {
        group,
        joining: false,
      } = reply
      else {
        panic!("{reply:?}");
      };
      assert_ne!(group, reviewed.previous);
      assert_eq!(
        state.root.configuration().home_of(object, RegionId(0)),
        RegionId(2)
      );
      assert!(
        state.root.propose(RootCommand::AdmitRegion(RegionId(4))),
        "the reviewed copy can commit under its new group"
      );
    });
  }

  /// AC-8.1 / T-2.14: authorize a join, restart before its target is reachable, and send
  /// old-group traffic. Refuse it while preserving the old copy for a new reviewed recovery.
  #[test]
  fn a_pending_recovery_join_survives_restart_without_serving_the_old_group() {
    use slates_cluster::raft::RequestVote;
    use slates_cluster::raft_wire::{RaftMessage, RaftWireError};
    use slates_wire::Wire;
    crate::daemon::audit_on_shard(|state| {
      let object = ObjectId([2; 16]);
      move_home(state, object);
      let local = state.fleet.host();
      let old_message = crate::consensus::encode_message(
        state,
        true,
        &RaftMessage::RequestVote(RequestVote {
          term: 7,
          candidate: local,
          last_log_index: 0,
          last_log_term: 0,
        }),
      )
      .unwrap();
      let fetch = crate::consensus::Fetch {
        group: None,
        version: 0,
      }
      .to_bytes();
      let old_fetch = crate::consensus::serve_fetch(state, true, &fetch).unwrap();
      let target = Some([3; 32]);
      let reviewed = plan(state, true, target).unwrap();
      let proof = recovery_proof(&state.issuer_secret, &reviewed.digest);
      assert!(matches!(
        recover(state, true, target, reviewed.digest, proof),
        ReplyBody::RecoveryStarted { joining: true, .. }
      ));
      crate::retention::load(&state.segment)
        .unwrap()
        .unwrap()
        .restore(state)
        .unwrap();
      assert_eq!(
        crate::consensus::decode_message(state, true, local, &old_message),
        Err(RaftWireError::ForeignGroup)
      );
      assert!(!crate::consensus::adopt_fetch(
        state, true, local, &old_fetch
      ));
      assert!(
        !state.root.propose(RootCommand::AdmitRegion(RegionId(4))),
        "a paused group has no proposing authority"
      );
      let reviewed = plan(state, true, None).unwrap();
      let proof = recovery_proof(&state.issuer_secret, &reviewed.digest);
      assert!(matches!(
        recover(state, true, None, reviewed.digest, proof),
        ReplyBody::RecoveryStarted { joining: false, .. }
      ));
      assert_eq!(
        state.root.configuration().home_of(object, RegionId(0)),
        RegionId(2)
      );
    });
  }
}
