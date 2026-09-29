//! First-time bootstrap and fresh-member admission (§4.8, AUD-07; Ongaro's dissertation §3.8).
//! A daemon never recovers a voter by recreating its empty state. Each start has a fresh identity;
//! a joining member imports the group's common base and prefix once, then advances through Raft.
//! Only an authenticated local account can explicitly create a new group. A missing quorum stays missing.

use slates_cluster::config_group::RegionalCouncil;
use slates_cluster::raft::{SavedRaft, VoterConfig};
use slates_cluster::raft_wire::{
  RaftMessage, RaftWireError, decode_regional_configuration, decode_root_configuration,
  encode_regional_configuration, encode_root_configuration,
};
use slates_cluster::root_group::RootGroup;
use slates_db::register::{HostId, Quorum, RegionId};
use slates_ipc::protocol::{Refusal, ReplyBody};
use slates_wire::Wire;

use crate::state::ShardState;

/// Creates a region's initial single-voter group. `root` additionally creates the fleet's root
/// group, once on its first node. Other regions require an existing root that has admitted them.
/// The leader subsequently adds reachable members through the ordinary joint-consensus path.
pub(crate) fn bootstrap(state: &mut ShardState, root: bool, member: u64) -> ReplyBody {
  let refuse = |refusal| ReplyBody::Refused { refusal };
  if state.fleet.host().0 != member {
    return refuse(Refusal::ConsensusBootstrapStale);
  }
  if state.bootstrap_authorized == Some(root) {
    return ReplyBody::Acknowledged;
  }
  if state.council.initialized() || (root && state.root.initialized()) {
    return refuse(Refusal::ConsensusAlreadyInitialized);
  }
  let local = state.fleet.host();
  let region = state
    .node_regions
    .get(&local)
    .copied()
    .unwrap_or(RegionId(0));
  if !root && (!state.root.initialized() || !state.root.configuration().regions.contains(&region)) {
    return refuse(Refusal::ConsensusNotInitialized);
  }
  let quorum = state
    .config
    .fleet
    .as_ref()
    .map_or(Quorum { f: 0 }, |fleet| fleet.quorum);
  let domains = state
    .config
    .fleet
    .as_ref()
    .and_then(|fleet| {
      fleet
        .domains
        .get(&crate::deploy::member_id(state.origin_anchor, 0))
        .copied()
    })
    .map(|domain| [(local, domain)].into())
    .unwrap_or_default();
  state.council = RegionalCouncil::new(
    local,
    vec![local],
    vec![local],
    quorum,
    domains,
    state.config.derived_scatter(quorum),
    false,
  );
  if root {
    state.root = RootGroup::new(local, vec![region], vec![local]);
  }
  // Install the newly created local authority immediately; fleet admission continues on its
  // coordinator, while the N=1 group already has its only voter and needs no network driver.
  if let Some(configuration) = state.council.configuration().configuration_for(local) {
    state.durability_shortfall = state
      .config
      .fleet
      .as_ref()
      .and_then(|fleet| fleet.durability)
      .and_then(|bound| bound.shortfall(&configuration));
    state.fleet.install_configuration(configuration, &[local]);
  }
  if let Some((raft, base)) = state.council.join_state() {
    state.council_group =
      GroupIdentity::created(false, &raft, encode_regional_configuration(&base));
  }
  if root && let Some((raft, base)) = state.root.join_state() {
    state.root_group = GroupIdentity::created(true, &raft, encode_root_configuration(&base));
  }
  state.consensus_ready = state.root.initialized();
  state.bootstrap_authorized = Some(root);
  ReplyBody::Acknowledged
}

/// A fetch distinguishes a fresh member, which needs the common base and consensus prefix,
/// from an initialized learner, which only needs a newer applied configuration.
#[derive(Wire)]
pub(crate) struct Fetch {
  pub group: Option<[u8; 32]>,
  pub version: u64,
}

/// The state donor and its prefix travel together, with the group's origin — which the joiner hashes
/// against the group id the reply names, since the donor's bases may have moved past it by compaction. The
/// caller checks the donor against the authenticated peer before decoding the group's application state.
#[derive(Wire)]
struct JoinState {
  raft: SavedRaft,
  base: Vec<u8>,
  origin: Origin,
}

#[derive(Wire)]
struct Fetched {
  group: [u8; 32],
  join: Option<JoinState>,
  configuration: Vec<u8>,
}

pub(crate) fn serve_fetch(state: &ShardState, root: bool, bytes: &[u8]) -> Option<Vec<u8>> {
  if state.recovery.target(root).is_some() {
    return None;
  }
  let request = Fetch::from_bytes(bytes).ok()?;
  let (initialized, version) = if root {
    (state.root.initialized(), state.root.configuration().version)
  } else {
    (
      state.council.initialized(),
      state.council.configuration().version,
    )
  };
  let identity = group_identity(state, root)?;
  let group = identity.id;
  if !initialized
    || request.group.is_some_and(|requested| requested != group)
    || (request.group.is_some() && request.version >= version)
  {
    return None;
  }
  let join = if request.group.is_some() {
    None
  } else if root {
    let (raft, base) = state.root.join_state()?;
    Some(JoinState {
      raft,
      base: encode_root_configuration(&base),
      origin: identity.origin.clone(),
    })
  } else {
    let (raft, base) = state.council.join_state()?;
    Some(JoinState {
      raft,
      base: encode_regional_configuration(&base),
      origin: identity.origin.clone(),
    })
  };
  let configuration = if root {
    encode_root_configuration(state.root.configuration())
  } else {
    encode_regional_configuration(state.council.configuration())
  };
  Some(
    Fetched {
      group,
      join,
      configuration,
    }
    .to_bytes(),
  )
}

/// A failed join leaves the member unable to vote. Once initialized, even a delayed initial
/// fetch cannot replace Raft state or erase a vote; only its newer applied view is considered.
pub(crate) fn adopt_fetch(state: &mut ShardState, root: bool, peer: HostId, bytes: &[u8]) -> bool {
  if !root && !crate::fleet::same_region(state, peer) {
    return false;
  }
  let Ok(reply) = Fetched::from_bytes(bytes) else {
    return false;
  };
  let pending = state.recovery.target(root).cloned();
  if pending
    .as_ref()
    .is_some_and(|target| target.group != reply.group)
    || (pending.is_none() && group_id(state, root).is_some_and(|group| group != reply.group))
  {
    return false;
  }
  if let Some(join) = &reply.join
    && genesis(root, &join.origin) != reply.group
  {
    return false;
  }
  if root {
    let Ok(configuration) = decode_root_configuration(&reply.configuration) else {
      return false;
    };
    if pending
      .as_ref()
      .is_some_and(|target| configuration.version < target.floor)
    {
      return false;
    }
    if pending.is_some() || !state.root.initialized() {
      let Some(join) = reply.join else {
        return false;
      };
      if join.raft.id != peer {
        return false;
      }
      let Ok(base) = decode_root_configuration(&join.base) else {
        return false;
      };
      let mut replacement = RootGroup::learner(state.fleet.host());
      if replacement.join_from(join.raft, base).is_err() {
        return false;
      }
      state.root = replacement;
      state.root_group = Some(GroupIdentity {
        id: reply.group,
        origin: join.origin,
      });
      state.recovery.root = None;
    }
    if !state.root.is_voter(state.fleet.host()) {
      state.root.adopt(configuration);
    }
  } else {
    let Ok(configuration) = decode_regional_configuration(&reply.configuration) else {
      return false;
    };
    if pending
      .as_ref()
      .is_some_and(|target| configuration.version < target.floor)
    {
      return false;
    }
    if pending.is_some() || !state.council.initialized() {
      let Some(join) = reply.join else {
        return false;
      };
      if join.raft.id != peer {
        return false;
      }
      let Ok(base) = decode_regional_configuration(&join.base) else {
        return false;
      };
      let mut replacement = RegionalCouncil::learner(
        state.fleet.host(),
        base.quorum,
        state.council.scatter(),
        base.has_mirror,
      );
      if replacement.join_from(join.raft, base).is_err() {
        return false;
      }
      state.council = replacement;
      state.council_group = Some(GroupIdentity {
        id: reply.group,
        origin: join.origin,
      });
      state.recovery.council = None;
    }
    if !state.council.is_voter(state.fleet.host()) {
      state.council.adopt(configuration);
    }
  }
  true
}

/// A configuration publication to owner shards. Raft roles and votes remain on the control
/// shard; the data path receives only the committed placement and its readiness bit.
#[derive(Clone)]
pub(crate) struct Publication {
  placement: slates_db::register::Configuration,
  members: Vec<HostId>,
  root: slates_db::register::RootConfiguration,
  ready: bool,
}

impl Publication {
  pub(crate) fn capture(state: &ShardState) -> Self {
    Self {
      placement: state.fleet.configuration().clone(),
      members: state.fleet.members().to_vec(),
      root: state.root.configuration().clone(),
      ready: state.consensus_ready,
    }
  }

  pub(crate) fn apply(self, state: &mut ShardState) {
    if self.placement.version >= state.fleet.configuration().version {
      state.durability_shortfall = state
        .config
        .fleet
        .as_ref()
        .and_then(|fleet| fleet.durability)
        .and_then(|bound| bound.shortfall(&self.placement));
      state
        .fleet
        .install_configuration(self.placement, &self.members);
      state.consensus_ready = self.ready;
    }
    state.root.adopt(self.root);
  }
}

/// The definition a consensus group was created with: its first voter configuration and the encoding of its
/// first application state. Its hash is the group's identity ([`genesis`]).
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
pub struct Origin {
  voters: VoterConfig,
  base: Vec<u8>,
}

/// A consensus group's immutable identity (§4.8, AUD-07): the hash of its [`Origin`], and the origin itself.
/// The hash binds every exchange to the group (the envelope). The origin travels with a join and is retained
/// with the group, so a donor's or a publication's claim is checked against the origin after compaction has
/// moved the Raft log's base and the fold's base past it — the check that hashed the current bases held only
/// while the groups never compacted (until 2026-09-28).
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
pub struct GroupIdentity {
  /// The group id: [`genesis`] of the origin.
  pub id: [u8; 32],
  /// What the group was created with.
  pub origin: Origin,
}

impl GroupIdentity {
  /// The identity of a group created now, from its current state: nothing has compacted it yet, so its Raft
  /// base voter configuration and its fold base are its origin. `None` for a state already compacted, which
  /// no creation produces.
  pub(crate) fn created(root: bool, raft: &SavedRaft, base: Vec<u8>) -> Option<GroupIdentity> {
    (raft.snapshot_index == 0).then(|| {
      let origin = Origin {
        voters: raft.base.clone(),
        base,
      };
      GroupIdentity {
        id: genesis(root, &origin),
        origin,
      }
    })
  }
}

/// A group's id: the hash of its origin — the same bytes, field for field, as before origins were carried.
pub(crate) fn genesis(root: bool, origin: &Origin) -> [u8; 32] {
  let mut hash = blake3::Hasher::new();
  hash.update(b"slates/consensus-genesis/v1");
  hash.update(&[u8::from(root)]);
  hash.update(&origin.voters.to_bytes());
  hash.update(&origin.base);
  *hash.finalize().as_bytes()
}

fn group_identity(state: &ShardState, root: bool) -> Option<&GroupIdentity> {
  if root {
    state.root_group.as_ref()
  } else {
    state.council_group.as_ref()
  }
}

fn group_id(state: &ShardState, root: bool) -> Option<[u8; 32]> {
  group_identity(state, root).map(|identity| identity.id)
}

/// Bind every consensus exchange to its immutable group, as well as its authenticated member.
#[derive(Wire)]
struct Envelope {
  group: [u8; 32],
  message: Vec<u8>,
}

/// Derived: the entry bytes one consensus append carries to a group of `voters` — a fresh fleet session's
/// first credit at the fleet frame cap, less the append's fixed header, its priority table and the
/// [`Envelope`] around it (`slates_cluster::raft_wire::append_batch_bytes`). The envelope's overhead is
/// measured from its own encoding, so the two cannot drift apart.
pub(crate) fn append_batch_bytes(voters: usize) -> usize {
  let envelope = Envelope {
    group: [0; 32],
    message: Vec::new(),
  }
  .to_bytes()
  .len();
  slates_cluster::raft_wire::append_batch_bytes(crate::fleet::FLEET_FRAME_CAP, envelope, voters)
}

pub(crate) fn encode_message(
  state: &ShardState,
  root: bool,
  message: &RaftMessage,
) -> Option<Vec<u8>> {
  Some(
    Envelope {
      group: group_id(state, root)?,
      message: message.encode(),
    }
    .to_bytes(),
  )
}

pub(crate) fn decode_message(
  state: &ShardState,
  root: bool,
  peer: HostId,
  bytes: &[u8],
) -> Result<RaftMessage, RaftWireError> {
  if state.recovery.target(root).is_some() {
    return Err(RaftWireError::ForeignGroup);
  }
  let group = group_id(state, root).ok_or(RaftWireError::Uninitialized)?;
  let envelope = Envelope::from_bytes(bytes).map_err(|_| RaftWireError::MalformedEnvelope)?;
  if envelope.group != group {
    return Err(RaftWireError::ForeignGroup);
  }
  RaftMessage::decode_from(&envelope.message, peer)
}
