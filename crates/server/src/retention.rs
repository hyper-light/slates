//! Anchor-owned Raft publications (§4.8, AUD-07; Raft Figure 2's persistence-before-reply rule).
//! The control shard publishes both groups and their member identity together before releasing a
//! transition's result. Two bounded slots preserve the last completed publication if a process dies
//! while writing the next. Complete but corrupt publications refuse recovery: an older vote is unsafe.

use slates_anchor::{AnchorError, AnchorSegment, RegionKind};
use slates_cluster::raft::SavedRaft;
use slates_cluster::raft_wire::{
  decode_regional_configuration, decode_root_configuration, encode_regional_configuration,
  encode_root_configuration,
};
use slates_db::register::{HostId, RegionId};
use slates_wire::Wire;

use crate::state::ShardState;

/// Format: two alternating publications, the completed state and its replacement.
const SLOTS: u8 = 2;
/// Format: a BLAKE3 digest precedes the encoded publication.
const CHECKSUM_BYTES: usize = 32;

#[derive(Clone, Wire)]
struct Group {
  raft: SavedRaft,
  base: Vec<u8>,
  view: Vec<u8>,
  genesis: [u8; CHECKSUM_BYTES],
  /// What the group was created with, whose hash the genesis is: compaction moves the retained bases past
  /// it, so the check hashes this rather than them.
  origin: crate::consensus::Origin,
}

#[derive(Clone, Wire)]
struct Region {
  host: HostId,
  region: u64,
}

/// Complete recovery input, read once before shard startup so no reader races publication.
#[derive(Clone, Wire)]
pub(crate) struct Retained {
  generation: u64,
  anchor: HostId,
  pub(crate) nonce: u64,
  scatter: u64,
  bootstrap: Option<bool>,
  recovery: crate::consensus_recovery::RecoveryState,
  enrolled: Vec<crate::discovery::Announcement>,
  council: Option<Group>,
  root: Option<Group>,
  regions: Vec<Region>,
}

fn invalid(reason: &'static str) -> AnchorError {
  AnchorError::Layout { reason }
}

impl Retained {
  fn capture(state: &ShardState, generation: u64) -> Self {
    Self {
      generation,
      anchor: state.origin_anchor,
      nonce: state.member_boot_nonce,
      scatter: state.council.scatter(),
      bootstrap: state.bootstrap_authorized,
      recovery: state.recovery.clone(),
      enrolled: state.enrolled.clone(),
      council: state
        .council
        .join_state()
        .zip(state.council_group.clone())
        .map(|((raft, base), identity)| Group {
          raft,
          base: encode_regional_configuration(&base),
          view: encode_regional_configuration(state.council.configuration()),
          genesis: identity.id,
          origin: identity.origin,
        }),
      root: state.root.join_state().zip(state.root_group.clone()).map(
        |((raft, base), identity)| Group {
          raft,
          base: encode_root_configuration(&base),
          view: encode_root_configuration(state.root.configuration()),
          genesis: identity.id,
          origin: identity.origin,
        },
      ),
      regions: state
        .node_regions
        .iter()
        .map(|(host, region)| Region {
          host: *host,
          region: region.0,
        })
        .collect(),
    }
  }

  pub(crate) fn restore(self, state: &mut ShardState) -> Result<(), AnchorError> {
    if self.anchor != state.origin_anchor || self.nonce != state.member_boot_nonce {
      return Err(invalid("retained consensus belongs to another identity"));
    }
    if let Some(group) = self.council {
      if crate::consensus::genesis(false, &group.origin) != group.genesis {
        return Err(invalid("retained council genesis differs"));
      }
      let base = decode_regional_configuration(&group.base)
        .map_err(|_| invalid("invalid retained council base"))?;
      let view = decode_regional_configuration(&group.view)
        .map_err(|_| invalid("invalid retained council view"))?;
      state
        .council
        .restore_from(group.raft, base)
        .map_err(|_| invalid("invalid retained council state"))?;
      state.council.adopt(view);
      state.council_group = Some(crate::consensus::GroupIdentity {
        id: group.genesis,
        origin: group.origin,
      });
    }
    if let Some(group) = self.root {
      if crate::consensus::genesis(true, &group.origin) != group.genesis {
        return Err(invalid("retained root genesis differs"));
      }
      let base = decode_root_configuration(&group.base)
        .map_err(|_| invalid("invalid retained root base"))?;
      let view = decode_root_configuration(&group.view)
        .map_err(|_| invalid("invalid retained root view"))?;
      state
        .root
        .restore_from(group.raft, base)
        .map_err(|_| invalid("invalid retained root state"))?;
      state.root.adopt(view);
      state.root_group = Some(crate::consensus::GroupIdentity {
        id: group.genesis,
        origin: group.origin,
      });
    }
    state.node_regions = self
      .regions
      .into_iter()
      .map(|entry| (entry.host, RegionId(entry.region)))
      .collect();
    state.bootstrap_authorized = self.bootstrap;
    state.recovery = self.recovery;
    state.enrolled = self.enrolled;
    if state.recovery.root.is_some() {
      state.root.suspend();
    }
    if state.recovery.council.is_some() {
      state.council.suspend();
    }
    state.consensus_generation = self.generation;
    let local = state.fleet.host();
    state.consensus_ready = !state.recovery.joining()
      && state.council.initialized()
      && state.root.initialized()
      && state.council.configuration().members.contains(&local);
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
    Ok(())
  }

  pub(crate) fn scatter(&self) -> u64 {
    self.scatter
  }
}

/// Reads the last completed publication. Only an explicitly unfinished slot may be skipped;
/// a damaged completed record is never replaced by an older term or vote.
pub(crate) fn load(segment: &AnchorSegment) -> Result<Option<Retained>, AnchorError> {
  let mut latest: Option<Retained> = None;
  let mut unfinished = false;
  for slot in 0..SLOTS {
    let payload = match segment.read_published(RegionKind::Consensus(slot)) {
      Ok(None) => continue,
      Ok(Some(payload)) => payload,
      Err(AnchorError::PublicationInProgress) => {
        unfinished = true;
        continue;
      }
      Err(error) => return Err(error),
    };
    let Some((checksum, bytes)) = payload.split_at_checked(CHECKSUM_BYTES) else {
      return Err(invalid("short retained consensus publication"));
    };
    if blake3::hash(bytes).as_bytes() != checksum {
      return Err(invalid("retained consensus checksum differs"));
    }
    let record =
      Retained::from_bytes(bytes).map_err(|_| invalid("invalid retained consensus publication"))?;
    if record.generation == 0 {
      return Err(invalid("zero retained consensus generation"));
    }
    if latest
      .as_ref()
      .is_none_or(|previous| previous.generation < record.generation)
    {
      latest = Some(record);
    }
  }
  if unfinished && latest.is_none() {
    return Err(invalid("initial consensus publication was interrupted"));
  }
  Ok(latest)
}

/// Publishes only changed consensus state, on its owning shard. A failed publication closes the
/// control shard before any response can escape; continuing under an unretained vote is unsafe.
pub(crate) fn retain(state: &mut ShardState) -> Result<(), AnchorError> {
  if let Some(error) = &state.consensus_failure {
    return Err(error.clone());
  }
  if state.shards.first() != Some(&state.shard)
    || (!state.council.retention_pending() && !state.root.retention_pending())
  {
    return Ok(());
  }
  publish_authorization(state)
}

/// An operator authorization changes participation even when the old Raft state is unchanged.
/// It uses the same bounded publication and failure rule as an ordinary Raft transition.
pub(crate) fn publish_authorization(state: &mut ShardState) -> Result<(), AnchorError> {
  if let Some(error) = &state.consensus_failure {
    return Err(error.clone());
  }
  let outcome = publish(state);
  if let Err(error) = &outcome {
    state.consensus_failure = Some(error.clone());
    state.consensus_ready = false;
    eprintln!("slates-server: consensus retention refused; control shard stopped: {error}");
  }
  outcome
}

fn publish(state: &mut ShardState) -> Result<(), AnchorError> {
  if state.council.initialized() != state.council_group.is_some()
    || state.root.initialized() != state.root_group.is_some()
  {
    return Err(invalid("initialized consensus has no matching genesis"));
  }
  let generation = state
    .consensus_generation
    .checked_add(1)
    .ok_or_else(|| invalid("consensus publication generation exhausted"))?;
  let record = Retained::capture(state, generation).to_bytes();
  let mut payload = Vec::with_capacity(CHECKSUM_BYTES.saturating_add(record.len()));
  payload.extend_from_slice(blake3::hash(&record).as_bytes());
  payload.extend_from_slice(&record);
  let slot = u8::try_from(generation % u64::from(SLOTS))
    .map_err(|_| invalid("consensus slot exceeds its format"))?;
  state
    .segment
    .publish(RegionKind::Consensus(slot), &payload)
    .map_err(|error| match error {
      AnchorError::ProfileTooLarge { offered, capacity } => {
        AnchorError::ConsensusCapacity { offered, capacity }
      }
      other => other,
    })?;
  state.consensus_generation = generation;
  state.council.retained();
  state.root.retained();
  set_log_budgets(state, payload.len(), slot);
  Ok(())
}

/// Format: the consensus groups whose logs grow the retained record — the regional council and the root
/// group — which share the record's remaining room.
const GROUPS_SHARING_THE_RECORD: usize = 2;

/// Sets each group's log budget from the record just published (AUD-29-37): the log it holds now plus an
/// equal share of the room the record leaves in its region. Both groups admitted together stay within the
/// region, growth since this publication counts against the share, and neither group can starve the other
/// of all the room. A won election's recovery that would not fit is declined before anything is appended,
/// rather than overflowing the next publication and closing the control shard.
fn set_log_budgets(state: &mut ShardState, record_bytes: usize, slot: u8) {
  let Ok(capacity) = state.segment.region_len(RegionKind::Consensus(slot)) else {
    return;
  };
  let share = capacity.saturating_sub(record_bytes) / GROUPS_SHARING_THE_RECORD;
  let council = state.council.log_bytes().saturating_add(share);
  let root = state.root.log_bytes().saturating_add(share);
  state.council.set_log_budget(council);
  state.root.set_log_budget(root);
}

/// Sets the log budgets at a shard's start from the record it restored (or would publish now), which no
/// publication has measured yet in this process: one encoding of the captured record, on the cold start path.
pub(crate) fn derive_log_budgets(state: &mut ShardState) {
  let record = Retained::capture(state, state.consensus_generation).to_bytes();
  let record_bytes = CHECKSUM_BYTES.saturating_add(record.len());
  let slot = u8::try_from(state.consensus_generation % u64::from(SLOTS)).unwrap_or(0);
  set_log_budgets(state, record_bytes, slot);
}

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used, clippy::panic)]

  use super::*;
  use slates_anchor::layout::{PAYLOAD_BYTES, PAYLOAD_LEN};
  use slates_cluster::config_group::RegionalCouncil;
  use slates_cluster::raft::RequestVote;
  use slates_cluster::raft_wire::RaftMessage;
  use slates_db::register::Quorum;

  fn vote(state: &mut ShardState, candidate: HostId, term: u64) -> bool {
    let reply = state
      .council
      .answer(RaftMessage::RequestVote(RequestVote {
        term,
        candidate,
        last_log_index: 0,
        last_log_term: 0,
      }))
      .unwrap();
    let RaftMessage::VoteReply(reply) = reply else {
      panic!("expected a vote reply")
    };
    reply.granted
  }

  fn retain_first_vote(state: &mut ShardState) {
    let local = state.fleet.host();
    let voters = vec![local, HostId(1), HostId(2)];
    state.council = RegionalCouncil::new(
      local,
      voters.clone(),
      voters,
      Quorum { f: 1 },
      Default::default(),
      3,
      false,
    );
    let (raft, base) = state.council.join_state().unwrap();
    state.council_group =
      crate::consensus::GroupIdentity::created(false, &raft, encode_regional_configuration(&base));
    assert!(vote(state, HostId(1), 7));
    retain(state).unwrap();
  }

  /// AUD-29-37 (§4.8). Do: retain a vote (a publication), then read both groups' log budgets. Expect: each
  /// is its log plus half the room the published record leaves in its region — so the two groups' logs,
  /// admitted together, fit the region — where before every budget was unbounded.
  #[test]
  fn a_publication_shares_the_records_room_between_the_groups_log_budgets() {
    crate::daemon::audit_on_shard(|state| {
      retain_first_vote(state);
      let slot = u8::try_from(state.consensus_generation % u64::from(SLOTS)).unwrap();
      let capacity = state
        .segment
        .region_len(RegionKind::Consensus(slot))
        .unwrap();
      let record = state
        .segment
        .read_published(RegionKind::Consensus(slot))
        .unwrap()
        .unwrap()
        .len();
      let share = (capacity - record) / GROUPS_SHARING_THE_RECORD;
      assert_eq!(
        state.council.log_budget(),
        state.council.log_bytes() + share
      );
      assert_eq!(state.root.log_budget(), state.root.log_bytes() + share);
      assert!(
        record
          + (state.council.log_budget() - state.council.log_bytes())
          + (state.root.log_budget() - state.root.log_bytes())
          <= capacity,
        "both groups' admitted growth fits the region"
      );
    });
  }

  /// AC-8.1 / T-2.14: stop a replacement publication after each payload byte; recover the
  /// acknowledged vote from the other slot and refuse another candidate in the same term.
  #[test]
  fn an_interrupted_publication_never_erases_an_acknowledged_vote() {
    crate::daemon::audit_on_shard(|state| {
      retain_first_vote(state);
      assert!(vote(state, HostId(2), 8));
      let next = Retained::capture(state, state.consensus_generation + 1).to_bytes();
      let mut payload = blake3::hash(&next).as_bytes().to_vec();
      payload.extend_from_slice(&next);
      let target = RegionKind::Consensus(
        u8::try_from((state.consensus_generation + 1) % u64::from(SLOTS)).unwrap(),
      );
      // The target's length and body as they stand (racy bytes, copied as the seqlock copies them).
      let mut saved_target = vec![0u8; PAYLOAD_BYTES - PAYLOAD_LEN + payload.len()];
      state
        .segment
        .region_read_racy(target, PAYLOAD_LEN, &mut saved_target)
        .unwrap();
      // Begin and commit are atomic words; the length and body can tear at any byte.
      for copied in 0..=payload.len() {
        let segment = &mut state.segment;
        segment
          .region_write_racy(target, PAYLOAD_LEN, &saved_target)
          .unwrap();
        segment
          .payload_generation(target)
          .unwrap()
          .store(1, std::sync::atomic::Ordering::Release);
        segment
          .region_write_racy(target, PAYLOAD_LEN, &(payload.len() as u64).to_le_bytes())
          .unwrap();
        segment
          .region_write_racy(target, PAYLOAD_BYTES, &payload[..copied])
          .unwrap();
        load(&state.segment)
          .unwrap()
          .unwrap()
          .restore(state)
          .unwrap();
        assert!(
          !vote(state, HostId(2), 7),
          "a crash after {copied} bytes erased a vote"
        );
      }
      // Complete that same replacement, then recover its newer vote too.
      state.segment.publish(target, &payload).unwrap();
      load(&state.segment)
        .unwrap()
        .unwrap()
        .restore(state)
        .unwrap();
      assert!(
        !vote(state, HostId(1), 8),
        "the completed replacement must take precedence"
      );
    });
  }

  /// §4.8 (AUD-07) with Raft §7: a council that has compacted its log restores from its publication with the
  /// same configuration under the same group id. The id is checked against the retained origin, because
  /// compaction moved the Raft base and the fold base past what the id was computed from; hashing them, as
  /// the check did before 2026-09-28, refuses every compacted group ("retained council genesis differs").
  #[test]
  fn a_compacted_council_restores_under_its_group_id() {
    use slates_cluster::config_group::Reconfiguration;
    crate::daemon::audit_on_shard(|state| {
      let local = state.fleet.host();
      state.council = RegionalCouncil::new(
        local,
        vec![local],
        vec![local],
        Quorum { f: 0 },
        Default::default(),
        3,
        false,
      );
      let (raft, base) = state.council.join_state().unwrap();
      state.council_group = crate::consensus::GroupIdentity::created(
        false,
        &raft,
        encode_regional_configuration(&base),
      );
      let id = state.council_group.as_ref().unwrap().id;
      for index in 0..120u64 {
        let host = HostId(1_000 + index / 2);
        let change = if index % 2 == 0 {
          Reconfiguration::Admit { host, domain: None }
        } else {
          Reconfiguration::Retire(host)
        };
        assert!(state.council.propose(change), "change {index}");
      }
      assert!(state.council.compactions() > 0, "the council compacted");
      assert!(state.council.snapshot_index() > 0);
      let expected = state.council.configuration().clone();
      retain(state).unwrap();
      load(&state.segment)
        .unwrap()
        .unwrap()
        .restore(state)
        .unwrap();
      assert_eq!(state.council.configuration(), &expected);
      assert_eq!(state.council_group.as_ref().unwrap().id, id);
    });
  }

  /// AC-8.1 / T-2.14: corrupt a completed publication; refuse recovery rather than restore
  /// the older slot, which would forget the most recently acknowledged vote.
  #[test]
  fn a_corrupt_completed_publication_cannot_fall_back_to_an_older_vote() {
    crate::daemon::audit_on_shard(|state| {
      retain_first_vote(state);
      let target =
        RegionKind::Consensus(u8::try_from(state.consensus_generation % u64::from(SLOTS)).unwrap());
      let flip = |segment: &mut slates_anchor::AnchorSegment| {
        let mut byte = [0u8; 1];
        segment
          .region_read_racy(target, PAYLOAD_BYTES + CHECKSUM_BYTES, &mut byte)
          .unwrap();
        byte[0] ^= 1;
        segment
          .region_write_racy(target, PAYLOAD_BYTES + CHECKSUM_BYTES, &byte)
          .unwrap();
      };
      flip(&mut state.segment);
      assert!(matches!(
        load(&state.segment),
        Err(AnchorError::Layout { .. })
      ));
      // Repair only the test's injected corruption so the fixture can shut down normally.
      flip(&mut state.segment);
    });
  }

  /// AC-8.1 / T-2.14: exceed the derived publication budget; refuse and retain the previous
  /// acknowledged vote. A refused publication cannot reopen the control shard on a later call.
  #[test]
  fn a_full_publication_slot_refuses_without_losing_the_previous_vote() {
    crate::daemon::audit_on_shard_configured(
      |state| {
        retain_first_vote(state);
        let (mut raft, base) = state.council.join_state().unwrap();
        let capacity = state.segment.region_len(RegionKind::Consensus(0)).unwrap();
        raft.log.push(slates_cluster::raft::LogEntry::command(
          raft.term,
          vec![0; capacity],
        ));
        state.council.restore_from(raft, base).unwrap();
        assert!(matches!(
          retain(state),
          Err(AnchorError::ConsensusCapacity { .. })
        ));
        assert!(
          matches!(retain(state), Err(AnchorError::ConsensusCapacity { .. })),
          "failure remains closed"
        );
        let recovered = load(&state.segment).unwrap().unwrap();
        // Simulate fresh process state only after proving that repeated calls remain refused.
        state.consensus_failure = None;
        recovered.restore(state).unwrap();
        assert!(!vote(state, HostId(2), 7));
      },
      |config| {
        config.geometry.log_bytes = config.geometry.page;
        config.geometry.snapshot_bytes = config.geometry.page;
      },
    );
  }
}
