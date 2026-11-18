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
    // The region's mirror scope exists exactly when the manifest declares it a mirror (§4.10; joiners receive it in
    // the group's base configuration, so every member agrees).
    state
      .region_mirrors
      .get(&region)
      .is_some_and(|mirror| *mirror != region),
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
  /// The answerer's configuration, which records the council's voters (A-372): how a learner, whose own Raft state never
  /// moves after its join, learns who votes now.
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

/// What folding a voter's fetch reply did to this node's group state ([`adopt_fetch`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FetchOutcome {
  /// The reply's configuration, or the join state it carried, was taken in (a configuration no newer than the
  /// one held changes nothing: the group only moves forward).
  Adopted,
  /// The voter had nothing newer for this node: [`serve_fetch`] answers a caught-up fetch, or one it will not
  /// serve, with no bytes. Neither adopted nor refused.
  Current,
  /// The reply was refused, counted under its reason and under [`JOIN_REFUSED`].
  Refused,
}

/// The status count under which a member records every fetch reply it refused, whatever the reason (each
/// reason is counted apart as well: [`JoinRefusal::counter`]).
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
pub(crate) const JOIN_REFUSED: &str = "consensus_join_refused";

/// Folds a voter's reply to this node's configuration fetch — on time or late, the one place a reply is
/// judged, so the two paths cannot disagree. A failed join leaves the member unable to vote. Once
/// initialized, even a delayed initial fetch cannot replace Raft state or erase a vote; only its newer
/// applied view is considered. An empty reply is the voter's "nothing newer" ([`FetchOutcome::Current`]).
pub(crate) fn adopt_fetch(
  state: &mut ShardState,
  root: bool,
  peer: HostId,
  bytes: &[u8],
) -> FetchOutcome {
  let refuse = |state: &mut ShardState, reason: JoinRefusal| {
    for name in [reason.counter(root), JOIN_REFUSED] {
      let count = state.refusals.entry(name).or_insert(0);
      *count = count.saturating_add(1);
    }
    FetchOutcome::Refused
  };
  if bytes.is_empty() {
    return FetchOutcome::Current;
  }
  if !root && !crate::fleet::same_region(state, peer) {
    return refuse(state, JoinRefusal::OtherRegion);
  }
  let Ok(reply) = Fetched::from_bytes(bytes) else {
    return refuse(state, JoinRefusal::ReplyUndecodable);
  };
  let pending = state.recovery.target(root).cloned();
  if pending
    .as_ref()
    .is_some_and(|target| target.group != reply.group)
    || (pending.is_none() && group_id(state, root).is_some_and(|group| group != reply.group))
  {
    return refuse(state, JoinRefusal::ForeignGroup);
  }
  if let Some(join) = &reply.join
    && genesis(root, &join.origin) != reply.group
  {
    return refuse(state, JoinRefusal::ForeignGenesis);
  }
  if root {
    let Ok(configuration) = decode_root_configuration(&reply.configuration) else {
      return refuse(state, JoinRefusal::ConfigurationUndecodable);
    };
    if pending
      .as_ref()
      .is_some_and(|target| configuration.version < target.floor)
    {
      return refuse(state, JoinRefusal::BelowFloor);
    }
    if pending.is_some() || !state.root.initialized() {
      let Some(join) = reply.join else {
        return refuse(state, JoinRefusal::NoJoinState);
      };
      if join.raft.id != peer {
        return refuse(state, JoinRefusal::JoinFromAnotherPeer);
      }
      let Ok(base) = decode_root_configuration(&join.base) else {
        return refuse(state, JoinRefusal::BaseUndecodable);
      };
      let mut replacement = RootGroup::learner(state.fleet.host());
      if replacement.join_from(join.raft, base).is_err() {
        return refuse(state, JoinRefusal::JoinRefused);
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
      return refuse(state, JoinRefusal::ConfigurationUndecodable);
    };
    if pending
      .as_ref()
      .is_some_and(|target| configuration.version < target.floor)
    {
      return refuse(state, JoinRefusal::BelowFloor);
    }
    if pending.is_some() || !state.council.initialized() {
      let Some(join) = reply.join else {
        return refuse(state, JoinRefusal::NoJoinState);
      };
      if join.raft.id != peer {
        return refuse(state, JoinRefusal::JoinFromAnotherPeer);
      }
      let Ok(base) = decode_regional_configuration(&join.base) else {
        return refuse(state, JoinRefusal::BaseUndecodable);
      };
      let mut replacement = RegionalCouncil::learner(
        state.fleet.host(),
        base.quorum,
        state.council.scatter(),
        base.has_mirror,
      );
      if replacement.join_from(join.raft, base).is_err() {
        return refuse(state, JoinRefusal::JoinRefused);
      }
      state.council = replacement;
      state.council_group = Some(GroupIdentity {
        id: reply.group,
        origin: join.origin,
      });
      state.recovery.council = None;
    }
    if !state.council.is_voter(state.fleet.host()) {
      // The voters travel inside the configuration (A-372), so a view adopted only forward moves them only forward.
      state.council.adopt(configuration);
    }
  }
  FetchOutcome::Adopted
}

/// Why a fetched configuration or join state was not adopted ([`adopt_fetch`]); each is counted under its own
/// name, so a refused join says which rule refused it.
#[derive(Clone, Copy)]
enum JoinRefusal {
  /// A regional fetch answered by a peer of another region.
  OtherRegion,
  /// The reply's bytes (not empty: an empty reply is [`FetchOutcome::Current`]) did not decode.
  ReplyUndecodable,
  /// The reply decoded, but the configuration it carries did not.
  ConfigurationUndecodable,
  /// The reply names a group other than the one this node belongs to or is recovering into.
  ForeignGroup,
  /// The join state's origin does not hash to the group the reply names.
  ForeignGenesis,
  /// The configuration is older than the recovery's floor.
  BelowFloor,
  /// An uninitialized node's fetch came back without the join state it needs.
  NoJoinState,
  /// The join state is another voter's, not the answering peer's.
  JoinFromAnotherPeer,
  /// The join state's base did not decode.
  BaseUndecodable,
  /// The group refused to install the join state (the saved log and its base disagree, or a reused identity).
  JoinRefused,
}

impl JoinRefusal {
  /// The refusal's status counter, per group.
  fn counter(self, root: bool) -> &'static str {
    match (root, self) {
      (false, Self::OtherRegion) => "consensus.join.other_region",
      (false, Self::ReplyUndecodable) => "consensus.join.reply_undecodable",
      (false, Self::ConfigurationUndecodable) => "consensus.join.configuration_undecodable",
      (false, Self::ForeignGroup) => "consensus.join.foreign_group",
      (false, Self::ForeignGenesis) => "consensus.join.foreign_genesis",
      (false, Self::BelowFloor) => "consensus.join.below_floor",
      (false, Self::NoJoinState) => "consensus.join.no_join_state",
      (false, Self::JoinFromAnotherPeer) => "consensus.join.from_another_peer",
      (false, Self::BaseUndecodable) => "consensus.join.base_undecodable",
      (false, Self::JoinRefused) => "consensus.join.refused_by_group",
      (true, Self::OtherRegion) => "consensus.root_join.other_region",
      (true, Self::ReplyUndecodable) => "consensus.root_join.reply_undecodable",
      (true, Self::ConfigurationUndecodable) => "consensus.root_join.configuration_undecodable",
      (true, Self::ForeignGroup) => "consensus.root_join.foreign_group",
      (true, Self::ForeignGenesis) => "consensus.root_join.foreign_genesis",
      (true, Self::BelowFloor) => "consensus.root_join.below_floor",
      (true, Self::NoJoinState) => "consensus.root_join.no_join_state",
      (true, Self::JoinFromAnotherPeer) => "consensus.root_join.from_another_peer",
      (true, Self::BaseUndecodable) => "consensus.root_join.base_undecodable",
      (true, Self::JoinRefused) => "consensus.root_join.refused_by_group",
    }
  }
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

#[cfg(test)]
mod tests {
  use std::collections::BTreeMap;

  use slates_cluster::config_group::RegionalCouncil;
  use slates_cluster::fleet::apply_peer_state;
  use slates_cluster::membership::{Liveness, MemberState};
  use slates_cluster::raft_wire::{encode_regional_configuration, encode_root_configuration};
  use slates_cluster::root_group::RootGroup;
  use slates_db::register::{HostId, Quorum, RegionId, RegionalConfiguration};
  use slates_wire::Wire;

  use super::{Fetch, FetchOutcome, GroupIdentity, adopt_fetch, serve_fetch};
  use crate::state::ShardState;

  /// Shape: the version of the configuration the region had committed when the fresh member's first fetch was
  /// answered. Any version past a learner's empty base (0) has the shape; 8 is the one the CI run showed
  /// (diag-raft-chaos-ab-68b-2: the fresh member's council at version 8 for the whole 48 s wait).
  const VERSION_BEFORE_ADMISSION: u64 = 8;

  /// A region's members as the council has them: the member the fresh node replaced and four others, the first of
  /// them the learner that answers the fresh node's first fetch.
  struct Region {
    /// The member the fresh node replaced: the bootstrap voter, gone.
    replaced: HostId,
    /// The four live members; `others[0]` is the donor.
    others: [HostId; 4],
    /// The fault tolerance the region runs at.
    quorum: Quorum,
  }

  impl Region {
    /// Every member before the fresh node's admission.
    fn members(&self) -> Vec<HostId> {
      let mut members = vec![self.replaced];
      members.extend(self.others);
      members
    }

    /// The configuration at `version` over `members`.
    fn configuration(
      &self,
      members: Vec<HostId>,
      version: u64,
      scatter: u64,
    ) -> RegionalConfiguration {
      let mut configuration =
        RegionalConfiguration::formed(members, self.quorum, BTreeMap::new(), scatter, false);
      configuration.version = version;
      configuration
    }
  }

  /// What the donor answered: its reply to the fresh node's first fetch (its join state and the configuration before
  /// the admission), and to a later fetch from version [`VERSION_BEFORE_ADMISSION`] (the configuration that admits it).
  struct Replies {
    council_first: Vec<u8>,
    root_first: Vec<u8>,
    council_admitted: Vec<u8>,
  }

  /// Puts this shard in the donor's place and asks it what [`serve_fetch`] answers: a learner of `region` that joined
  /// from the replaced bootstrapper, so the voters it knows (its join state's, in both groups) are that member alone.
  fn donor_answers(
    state: &mut ShardState,
    region: &Region,
    local: HostId,
    scatter: u64,
  ) -> Replies {
    let donor = region.others[0];
    let mut council = RegionalCouncil::new(
      donor,
      region.members(),
      vec![region.replaced],
      region.quorum,
      BTreeMap::new(),
      scatter,
      false,
    );
    let (raft, base) = council.join_state().unwrap();
    state.council_group =
      GroupIdentity::created(false, &raft, encode_regional_configuration(&base));
    assert!(council.adopt(region.configuration(
      region.members(),
      VERSION_BEFORE_ADMISSION,
      scatter
    )));
    let root = RootGroup::new(donor, vec![RegionId(0)], vec![region.replaced]);
    let (raft, base) = root.join_state().unwrap();
    state.root_group = GroupIdentity::created(true, &raft, encode_root_configuration(&base));
    state.council = council;
    state.root = root;
    let first = Fetch {
      group: None,
      version: 0,
    }
    .to_bytes();
    let council_first = serve_fetch(state, false, &first).unwrap();
    let root_first = serve_fetch(state, true, &first).unwrap();
    let mut admitted = region.members();
    admitted.push(local);
    assert!(state.council.adopt(region.configuration(
      admitted,
      VERSION_BEFORE_ADMISSION + 1,
      scatter
    )));
    let later = Fetch {
      group: state.council_group.as_ref().map(|identity| identity.id),
      version: VERSION_BEFORE_ADMISSION,
    }
    .to_bytes();
    let council_admitted = serve_fetch(state, false, &later).unwrap();
    Replies {
      council_first,
      root_first,
      council_admitted,
    }
  }

  /// Puts this shard back in the fresh node's place, a member of no group yet, and folds the donor's answers to its
  /// first fetch, as its learner does; its membership holds the four others alive and the replaced member dead.
  fn join_from_donor(
    state: &mut ShardState,
    region: &Region,
    replies: &Replies,
  ) -> [FetchOutcome; 2] {
    let local = state.fleet.host();
    let scatter = state.council.scatter();
    state.council = RegionalCouncil::learner(local, region.quorum, scatter, false);
    state.council_group = None;
    state.root = RootGroup::learner(local);
    state.root_group = None;
    let donor = region.others[0];
    let outcomes = [
      adopt_fetch(state, false, donor, &replies.council_first),
      adopt_fetch(state, true, donor, &replies.root_first),
    ];
    let alive = MemberState {
      liveness: Liveness::Alive,
      incarnation: 0,
    };
    for peer in region.others {
      apply_peer_state(&mut state.fleet, peer, Some(alive));
    }
    let dead = MemberState {
      liveness: Liveness::Dead,
      incarnation: 0,
    };
    apply_peer_state(&mut state.fleet, region.replaced, Some(dead));
    outcomes
  }

  /// Each live member's contact reason, and the members the probe mesh counts in contact.
  type Contact = (Vec<(HostId, String)>, usize);

  fn contact(state: &ShardState, region: &Region) -> Contact {
    (
      region
        .others
        .iter()
        .map(|peer| (*peer, crate::fleet::direct_contact_reason(state, *peer)))
        .collect(),
      crate::fleet::probe_mesh(state).in_contact.len(),
    )
  }

  /// What the fresh node held after its first fetch: whether both groups joined, the configuration's version, whether
  /// it names this node, and the voters it knows.
  type Joined = ([FetchOutcome; 2], u64, bool, Vec<HostId>);

  /// AC-8.1, §4.8 learners and direct contact (A-331): a fresh member whose first fetch was answered before the council
  /// admitted it keeps direct contact with every member it holds alive, so its learner has sessions to fetch the
  /// configuration that names it; once it holds one, the member rule applies again. Do: join this node, through the
  /// real fetch path, from a learner's answer that carries the configuration at version 8 (which does not name it) and
  /// names as voters only the bootstrapper this node replaced (dead here), and hold the four others alive; then fold a
  /// later answer whose configuration admits it. Expect each of the four kept, and the probe mesh counting four, while
  /// unadmitted; and no peer held `Unadmitted` once admitted. Found by live_chaos's fresh-member flow (2 of 48 runs at
  /// one CPU on the base, 1 of 48 with learner-3; main's ubuntu-latest run 44269183565 and macOS on 2026-11-18): the
  /// fresh member held all four `OutsideNeighbourhood`, its every record link idled (`fleet.link.out_of_contact` 4), its
  /// learner found no session for the whole wait (`fleet.learner.no_session` 113 in 48 s, diag-raft-chaos-ab-68b-2),
  /// and it never learned the configuration the others had admitted it in.
  #[test]
  fn a_fresh_member_joined_before_its_admission_keeps_contact_with_every_live_member() {
    let (joined, unadmitted, admitted) = crate::daemon::audit_on_shard(|state| {
      let local = state.fleet.host();
      let region = Region {
        replaced: HostId(local.0 ^ 1),
        others: [
          HostId(local.0 ^ 2),
          HostId(local.0 ^ 3),
          HostId(local.0 ^ 4),
          HostId(local.0 ^ 5),
        ],
        quorum: Quorum { f: 1 },
      };
      let scatter = state.config.derived_scatter(region.quorum);
      let replies = donor_answers(state, &region, local, scatter);
      let outcomes = join_from_donor(state, &region, &replies);
      let joined: Joined = (
        outcomes,
        state.council.configuration().version,
        state.council.configuration().members.contains(&local),
        state.council.known_voters(),
      );
      let unadmitted = contact(state, &region);
      let donor = region.others[0];
      let outcome = adopt_fetch(state, false, donor, &replies.council_admitted);
      let admitted = (
        outcome,
        state.council.configuration().members.contains(&local),
        contact(state, &region),
      );
      (joined, unadmitted, admitted)
    });
    let (outcomes, version, named, known_voters) = &joined;
    assert!(
      *outcomes == [FetchOutcome::Adopted, FetchOutcome::Adopted]
        && *version == VERSION_BEFORE_ADMISSION
        && !named
        && known_voters.len() == 1,
      "the fresh node joined both groups at version {VERSION_BEFORE_ADMISSION}, which does not name it, knowing only the \
       replaced bootstrapper as a voter: {joined:?}"
    );
    let (reasons, in_contact) = &unadmitted;
    assert!(
      reasons
        .iter()
        .all(|(_, reason)| !reason.starts_with("OutsideNeighbourhood")
          && !reason.starts_with("BelievedDead"))
        && *in_contact == reasons.len(),
      "a node its configuration does not name keeps contact with every member it holds alive, (peer, reason) and the \
       mesh's count: {unadmitted:?}"
    );
    let (outcome, named, (reasons, _)) = &admitted;
    assert!(
      *outcome == FetchOutcome::Adopted
        && *named
        && reasons
          .iter()
          .all(|(_, reason)| !reason.starts_with("Unadmitted")),
      "once a configuration names it, the member rule applies again: {admitted:?}"
    );
  }
}
