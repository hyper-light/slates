//! The root configuration group (§4.8 "Configuration, by consensus", D-14 — "a root group across regions
//! holds region membership and cross-region promotions") — the cross-region counterpart of the
//! [`RegionalCouncil`](crate::config_group::RegionalCouncil). Where a regional council agrees on a region's
//! host membership, neighbourhoods and epochs, the **root group** agrees on the fleet's `RootConfiguration`:
//! which regions exist, the home region of each moved volume, and the region a lost region is promoted to. It
//! is touched **only** on a region-membership change, a home move, or a region-loss promotion — never on a
//! per-write path (banned item 10) — and its commit rate is near zero, a tripwire.
//!
//! The configuration is the deterministic fold of a **committed Raft log** ([`crate::raft`], the hecate
//! dialect), exactly as the regional council's is: each change is a [`RootCommand`] the leader proposes and
//! every voter applies once committed, and the version bumps once per applied change. Degenerate on a laptop
//! (the sole region): the single voter self-elects and its append commits at once, the identical code path a
//! multi-region root group runs through replication, never a mode switch (R8).
//!
//! [`RootGroup`] is one node's participant in that consensus. Its Raft voters are the **hosts** that carry
//! the root group (a small set, one or a few per region — the same small-elected-set shape as a regional
//! council's voters); its committed configuration names **regions**. It is sans-io: the drive (election and
//! replication over the cross-region transport) lives in the daemon, exactly as the regional council's drive
//! does. This module realizes the root group's state machine and consensus; wiring it to the cross-region
//! transport is the daemon's, and is owed.
//!
//! The **voter set follows the committed regions**: the root voters are the representatives of the regions
//! in the committed root configuration ([`root_representatives`] — the lowest-id live host of each
//! region), so when a committed retirement or promotion drops a region, or a region's representative host
//! changes, the leader moves the Raft voter set to match through the core's joint-consensus change
//! ([`reconcile_voters`](RootGroup::reconcile_voters)); a dead representative stops counting toward every
//! root majority. Before this the voter set was fixed at boot and never shrank
//! (`docs/bugs/2026-09-13-raft-voter-set-never-shrinks.md`).

use std::collections::BTreeMap;
use std::mem::size_of;

use slates_db::register::{HostId, OBJECT_BYTES, ObjectId, RegionId, RootConfiguration};

use crate::raft::{AppendEntries, RaftNode};
use crate::raft_wire::RaftMessage;

/// A root-configuration change as it rides the Raft log — the command a committed
/// [`LogEntry`](crate::raft::LogEntry) carries, decoded and applied to the [`RootConfiguration`] in commit
/// order so every voter reaches the same configuration. The Raft core treats it as opaque bytes; this is the
/// root group's interpretation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootCommand {
  /// Admit a region to the fleet (a region joins).
  AdmitRegion(RegionId),
  /// Retire a region cleanly (an operator removal — no promotion, nothing served from a mirror).
  RetireRegion(RegionId),
  /// Promote `mirror` to serve a **lost** region (§4.8 "region loss promotes the mirror through the root
  /// group"): the lost region drops from the membership and its volumes are thereafter served from `mirror`.
  PromoteRegion {
    /// The lost region.
    lost: RegionId,
    /// The region promoted to serve it.
    mirror: RegionId,
  },
  /// Move a volume's home to another region (§4.8 "the homes of moved volumes"; "a region-home move also
  /// carries root-group authority").
  MoveHome {
    /// The volume whose home moves.
    volume: ObjectId,
    /// The region it moves to.
    to: RegionId,
  },
}

/// Format: a root command is a one-byte tag followed by its little-endian fields; these are the tags.
const COMMAND_ADMIT_REGION: u8 = 0;
const COMMAND_RETIRE_REGION: u8 = 1;
const COMMAND_PROMOTE_REGION: u8 = 2;
const COMMAND_MOVE_HOME: u8 = 3;

impl RootCommand {
  /// The command's canonical bytes for the log: the tag, then its fields little-endian (a region id is a
  /// u64; a moved home is the 16-byte volume id then the destination region).
  pub fn encode(&self) -> Vec<u8> {
    let mut out = Vec::new();
    match self {
      RootCommand::AdmitRegion(region) => {
        out.push(COMMAND_ADMIT_REGION);
        out.extend_from_slice(&region.0.to_le_bytes());
      }
      RootCommand::RetireRegion(region) => {
        out.push(COMMAND_RETIRE_REGION);
        out.extend_from_slice(&region.0.to_le_bytes());
      }
      RootCommand::PromoteRegion { lost, mirror } => {
        out.push(COMMAND_PROMOTE_REGION);
        out.extend_from_slice(&lost.0.to_le_bytes());
        out.extend_from_slice(&mirror.0.to_le_bytes());
      }
      RootCommand::MoveHome { volume, to } => {
        out.push(COMMAND_MOVE_HOME);
        out.extend_from_slice(&volume.0);
        out.extend_from_slice(&to.0.to_le_bytes());
      }
    }
    out
  }

  /// Decodes a command from a committed log entry, or `None` if the bytes are malformed (a corrupt log
  /// entry — never expected from our own [`encode`](RootCommand::encode), applied as a no-op if seen).
  pub fn decode(bytes: &[u8]) -> Option<RootCommand> {
    let (&tag, rest) = bytes.split_first()?;
    match tag {
      COMMAND_ADMIT_REGION => Some(RootCommand::AdmitRegion(take_region(rest)?.0)),
      COMMAND_RETIRE_REGION => Some(RootCommand::RetireRegion(take_region(rest)?.0)),
      COMMAND_PROMOTE_REGION => {
        let (lost, rest) = take_region(rest)?;
        let (mirror, _) = take_region(rest)?;
        Some(RootCommand::PromoteRegion { lost, mirror })
      }
      COMMAND_MOVE_HOME => {
        let (volume, rest) = take_object(rest)?;
        let (to, _) = take_region(rest)?;
        Some(RootCommand::MoveHome { volume, to })
      }
      _ => None,
    }
  }
}

/// Reads a u64 region id at the front of `bytes`, returning it and the remainder, or `None` if truncated.
fn take_region(bytes: &[u8]) -> Option<(RegionId, &[u8])> {
  if bytes.len() < size_of::<u64>() {
    return None;
  }
  let (head, rest) = bytes.split_at(size_of::<u64>());
  let mut word = [0u8; size_of::<u64>()];
  word.copy_from_slice(head);
  Some((RegionId(u64::from_le_bytes(word)), rest))
}

/// Reads an object (volume) id at the front of `bytes`, returning it and the remainder, or `None`.
fn take_object(bytes: &[u8]) -> Option<(ObjectId, &[u8])> {
  if bytes.len() < OBJECT_BYTES {
    return None;
  }
  let (head, rest) = bytes.split_at(OBJECT_BYTES);
  let mut id = [0u8; OBJECT_BYTES];
  id.copy_from_slice(head);
  Some((ObjectId(id), rest))
}

/// Each region's **representative** host among `hosts` — the lowest host id in the region — the one host
/// per region that carries the root group's consensus (§4.8, D-14 — "a small set, one or a few per
/// region"). A host absent from `regions` is in the sole region `RegionId(0)`. Deterministic from the hosts
/// and their regions, so every node derives the same representatives: at boot from the manifest's members
/// (the initial root voters), and each period from the hosts currently alive (the target
/// [`RootGroup::reconcile_voters`] moves the voter set to, so a region whose representative died is carried
/// by its next host).
pub fn root_representatives(
  hosts: &[HostId],
  regions: &BTreeMap<HostId, RegionId>,
) -> BTreeMap<RegionId, HostId> {
  let mut by_region: BTreeMap<RegionId, HostId> = BTreeMap::new();
  for &host in hosts {
    let region = regions.get(&host).copied().unwrap_or(RegionId(0));
    by_region
      .entry(region)
      .and_modify(|representative| {
        if host.0 < representative.0 {
          *representative = host;
        }
      })
      .or_insert(host);
  }
  by_region
}

/// The **root configuration group** on one node (§4.8, D-14 — the root group across regions): the multi-voter
/// [`RaftNode`] over the root-group hosts, producing the [`RootConfiguration`] every region learns. A change
/// commits at a majority over the (cross-region) transport — the leader [`propose`](RootGroup::propose)s, the
/// followers serve, and each committed command applies to the root configuration. The drive primitives ride
/// the same [`RaftMessage`] wire the transport carries, exactly as the regional council's do.
pub struct RootGroup {
  raft: RaftNode,
  configuration: RootConfiguration,
  /// The formed configuration every voter's fold starts from (see the regional council's `base`): a node
  /// promoted to voter re-folds the whole log from this, never on top of an adopted configuration.
  base: RootConfiguration,
  applied: u64,
  /// Monotone count of the events that defer this node's own election — a leader's append answered here, or a
  /// vote this node granted a candidate (Raft Figure 2's two follower timer-resets). The drive loop's election
  /// timer reads it: while it advances, either a leader is alive or a candidate this node backed is still
  /// contesting the election, so this node does not campaign; once it stalls for the election timeout, contact
  /// is presumed lost and a pre-election begins.
  leader_contact: u64,
}

impl RootGroup {
  /// A root group on this `node` (one of the `voters` — the hosts that carry the root group) holding the
  /// fleet's initial `regions`. A multi-voter group waits for an election over the transport (the drive
  /// loop); a **sole voter** self-elects at once (the laptop's single-region degenerate — the same root-group
  /// code a multi-region fleet runs, immediately its own leader so it may propose with no messages, R8).
  pub fn new(node: HostId, regions: Vec<RegionId>, voters: Vec<HostId>) -> RootGroup {
    let mut raft = RaftNode::new(node, voters);
    // A group of one is immediately its own majority: elect at once so the laptop's root group is the
    // authority with no drive loop. A multi-voter group must win a real election over the transport.
    if raft.all_voters().len() == 1 {
      let _ = raft.start_election();
    }
    let configuration = RootConfiguration::formed(regions);
    RootGroup {
      raft,
      base: configuration.clone(),
      configuration,
      applied: 0,
      leader_contact: 0,
    }
  }

  /// The root configuration the group has agreed on so far — every region derives its home lookups from it
  /// ([`RootConfiguration::home_of`]).
  pub fn configuration(&self) -> &RootConfiguration {
    &self.configuration
  }

  /// Whether this node leads the root group (only the leader may propose).
  pub fn is_leader(&self) -> bool {
    self.raft.is_leader()
  }

  /// The host that currently leads the root group — itself when it leads, else the last leader it heard from
  /// (`None` while unsettled). A redirection hint for an operator command that must reach the leader (a
  /// region-loss promotion); a stale hint costs a retry, never a safety violation.
  pub fn leader(&self) -> Option<HostId> {
    self.raft.leader()
  }

  /// The peers the drive loop ships elections and replication to: the group's current voters, plus — while a
  /// membership change's entry is still uncommitted — the voters it is removing, so a live host demoted from
  /// representative receives the entry that removes it ([`RaftNode::replication_targets`]).
  pub fn voters(&self) -> Vec<HostId> {
    self.raft.replication_targets()
  }

  /// The leader-contact count (see the field): the drive loop's election timer resets while this advances.
  pub fn leader_contact(&self) -> u64 {
    self.leader_contact
  }

  /// **DRIVE**: begins a **pre-election** on an election timeout (Raft §9.6), returning the [`PreVote`]s to
  /// ship to the other voters — asked without inflating the term, so a partitioned node cannot disrupt a
  /// healthy leader. A lone voter proceeds straight to leading with no messages (the degenerate case).
  pub fn election_timeout(&mut self) -> Vec<RaftMessage> {
    self
      .raft
      .on_election_timeout()
      .into_iter()
      .map(RaftMessage::PreVote)
      .collect()
  }

  /// The append the leader replicates to `follower` now (or a heartbeat), or `None` when not the leader.
  pub fn replication_for(&self, follower: HostId) -> Option<AppendEntries> {
    self.raft.replicate_to(follower)
  }

  /// **DRIVE**: the root leader's CheckQuorum tick (Raft §6.2), on the election-timeout cadence — the root
  /// group's parallel of [`RegionalCouncil::check_quorum`](crate::config_group::RegionalCouncil::check_quorum):
  /// a leader that has not heard from a majority of root voters since the previous tick steps down and the
  /// contact window resets; a non-leader is unaffected; the sole root voter (a single-region fleet) never
  /// steps down.
  pub fn check_quorum(&mut self) {
    self.raft.check_quorum();
  }

  /// **SERVE**: answers a request received over the transport — a pre-vote, a vote request, or an append —
  /// returning the reply to ship back and applying whatever newly committed to the root configuration (a
  /// follower applies on the append). A reply is not a request and is not answered here — its sender folds it
  /// with [`fold_reply`](RootGroup::fold_reply).
  pub fn answer(&mut self, request: RaftMessage) -> Option<RaftMessage> {
    match request {
      RaftMessage::PreVote(pre) => Some(RaftMessage::PreVoteReply(self.raft.on_pre_vote(pre))),
      RaftMessage::RequestVote(vote) => {
        let reply = self.raft.on_request_vote(vote);
        if reply.granted {
          // Granting a vote defers this node's own election (Raft §5.2, Figure 2 "Rules for Servers →
          // Followers": the election timer resets on *granting a vote* as well as on a current leader's
          // append). So the candidate this node just voted for has a full election timeout to win and send
          // its first heartbeat before this node would campaign in competition. Without this reset a fleet
          // livelocks under heavy CPU load: a newly elected leader is slow to send its first append (its
          // coordinator loop is starved for the core), the voters that elected it keep aging out and campaign
          // against it, and leadership never settles (docs/bugs/2026-09-13-election-timer-not-reset-on-vote-grant.md).
          self.leader_contact = self.leader_contact.saturating_add(1);
        }
        Some(RaftMessage::VoteReply(reply))
      }
      RaftMessage::AppendEntries(append) => {
        let append_term = append.term;
        let reply = self.raft.on_append_entries(append);
        // Any append from a current-or-newer-term leader is contact from the leader — reset the election timer
        // (Raft §5.2, Figure 2 "Followers": the timer resets on *receiving AppendEntries from the current
        // leader"). This holds even when the log-consistency check rejects the append (`reply.success == false`
        // while the follower is still catching its log up): the leader is live and this node must not campaign
        // against it — `on_append_entries` has already set `has_leader`, so this node also refuses others'
        // pre-votes, and gating the timer on `success` instead would leave it aging while it defends the leader,
        // campaigning uselessly (its own pre-vote refused by that leader) until it catches up. A stale-term
        // append (`append_term < reply.term`, from a deposed leader) is not contact and does not reset.
        if append_term >= reply.term {
          self.leader_contact = self.leader_contact.saturating_add(1);
        }
        self.apply_committed();
        Some(RaftMessage::AppendReply(reply))
      }
      RaftMessage::VoteReply(_) | RaftMessage::PreVoteReply(_) | RaftMessage::AppendReply(_) => {
        None
      }
    }
  }

  /// **DRIVE**: folds a reply to one of this node's requests, returning any follow-on messages to ship — a
  /// granted pre-vote majority yields the real [`RequestVote`]s (the pre-election succeeded, so the term is
  /// advanced only now); a vote reply or append reply yields none. Applies whatever newly committed to the
  /// root configuration on an append reply (the leader once a majority acknowledges).
  pub fn fold_reply(&mut self, reply: RaftMessage) -> Vec<RaftMessage> {
    match reply {
      RaftMessage::PreVoteReply(reply) => self
        .raft
        .on_pre_vote_reply(reply)
        .map(|votes| votes.into_iter().map(RaftMessage::RequestVote).collect())
        .unwrap_or_default(),
      RaftMessage::VoteReply(reply) => {
        self.raft.on_vote_reply(reply);
        Vec::new()
      }
      RaftMessage::AppendReply(reply) => {
        self.raft.on_append_reply(reply);
        self.apply_committed();
        Vec::new()
      }
      RaftMessage::PreVote(_) | RaftMessage::RequestVote(_) | RaftMessage::AppendEntries(_) => {
        Vec::new()
      }
    }
  }

  /// Proposes a root-configuration change on the leader **without waiting**: it commits — and applies to the
  /// root configuration — only once a majority of the group acknowledge it over the transport. Returns whether
  /// the leader appended it (a non-leader, or a change already reflected in the configuration, returns
  /// `false`, so the log grows only for real changes).
  pub fn propose(&mut self, command: RootCommand) -> bool {
    let would_change = match command {
      RootCommand::AdmitRegion(region) => !self.configuration.regions.contains(&region),
      RootCommand::RetireRegion(region) => self.configuration.regions.contains(&region),
      RootCommand::PromoteRegion { lost, mirror } => {
        lost != mirror
          && self.configuration.regions.contains(&lost)
          && self.configuration.regions.contains(&mirror)
      }
      RootCommand::MoveHome { volume, to } => {
        self.configuration.regions.contains(&to)
          && self.configuration.homes.get(&volume) != Some(&to)
      }
    };
    if !would_change {
      return false;
    }
    self.raft.append_command(command.encode())
  }

  /// Whether the group's log is fully committed — nothing proposed is still in flight
  /// (`last_log_index == commit_index`). A reconcile reads it so a change proposed but not yet committed is not
  /// re-proposed each period (the same caught-up gate the regional council uses).
  pub fn caught_up(&self) -> bool {
    self.raft.last_log_index() == self.raft.commit_index()
  }

  /// Reconciles the region membership with the set of regions currently **alive** — as the leader — the
  /// cross-region counterpart of [`RegionalCouncil::reconcile_alive`](crate::config_group::RegionalCouncil::reconcile_alive):
  /// proposes admitting every alive region not yet a member, and retiring a member region no host is alive in
  /// **only when it has no mirror** in `mirrors` — each through the root log so it commits at a majority over
  /// the transport and applies on every voter. Returns whether anything was proposed. A non-leader proposes
  /// nothing (only the elected root master decides), and the [`caught_up`](RootGroup::caught_up) gate keeps a
  /// change in flight from being re-proposed, so the log grows only for real changes.
  ///
  /// A lost region **with a mirror is left in place** for a deliberate operator promotion (§4.8 "region loss
  /// promotes the mirror through the root group at operator cadence"): auto-failing-over a region that is
  /// merely partitioned would promote its mirror while it is still serving — a second owner (split-brain). A
  /// lost region **without a mirror** has no failover target, so its confirmed loss is a clean retirement (no
  /// second owner is created, and SWIM re-admits it on recovery).
  pub fn reconcile_regions(
    &mut self,
    alive: &[RegionId],
    mirrors: &std::collections::BTreeMap<RegionId, RegionId>,
  ) -> bool {
    if !self.is_leader() || !self.caught_up() {
      return false;
    }
    let mut proposed = false;
    for region in alive {
      proposed |= self.propose(RootCommand::AdmitRegion(*region));
    }
    let stale: Vec<RegionId> = self
      .configuration
      .regions
      .iter()
      .copied()
      .filter(|region| !alive.contains(region))
      .collect();
    for region in stale {
      // A lost region that has a **mirror** is not auto-retired: cross-region failover is deliberate
      // ("region loss promotes the mirror through the root group at operator cadence", §4.8), because a region
      // that is merely partitioned — not truly lost — would otherwise be failed over while it is still
      // serving, promoting a second owner (split-brain). It stays in the membership, unreachable, until an
      // operator promotes it (`PromoteRegion`, driven by the daemon's operator control). A lost region with
      // **no** mirror has no failover target, so its confirmed loss is a clean retirement — no second owner is
      // ever created, and SWIM re-admits the region if it recovers.
      if mirrors.contains_key(&region) {
        continue;
      }
      proposed |= self.propose(RootCommand::RetireRegion(region));
    }
    proposed
  }

  /// Whether `node` is a **voter** of this root group — a host of its Raft consensus set in effect now. A
  /// non-voter learns the committed root configuration by fetching it and [`adopt`](RootGroup::adopt)ing it.
  /// A voter a committed change removed is no longer one.
  pub fn is_voter(&self, node: HostId) -> bool {
    self.raft.is_voter(node)
  }

  /// **DRIVE** (leader): keeps the root group's Raft voter set equal to the representatives of the regions in
  /// the committed root configuration — `representatives` is the caller's current map of each alive region
  /// to its representative host ([`root_representatives`] over the hosts alive now) — one joint change at a
  /// time (Raft §6), exactly as [`RegionalCouncil::reconcile_voters`](crate::config_group::RegionalCouncil::reconcile_voters):
  /// begin the joint change when the target differs from the voters in force, complete it once the joint
  /// entry has committed, no-op once `C_new` has. A committed region without a representative in the map
  /// (no host of it alive yet) contributes no voter; an empty target is never proposed. Returns whether it
  /// appended a configuration entry. A dead representative thereby leaves the root consensus set; a region
  /// whose representative died is carried by its next live host once the caller's map names it.
  pub fn reconcile_voters(&mut self, representatives: &BTreeMap<RegionId, HostId>) -> bool {
    if !self.is_leader() || !self.caught_up() {
      return false;
    }
    if self.raft.in_joint_configuration() {
      return self.raft.complete_membership_change();
    }
    let mut target: Vec<HostId> = self
      .configuration
      .regions
      .iter()
      .filter_map(|region| representatives.get(region).copied())
      .collect();
    target.sort_unstable_by_key(|host| host.0);
    target.dedup();
    if target.is_empty() || target == self.raft.all_voters() {
      return false;
    }
    self.raft.begin_membership_change(target)
  }

  /// Adopts a root configuration fetched from a group voter (§4.8, D-14: the root group is a small elected
  /// set, so a region that does not vote learns the committed configuration rather than voting on it). Returns
  /// whether it advanced — a fetch not newer than what this node holds is ignored, so adoption only moves
  /// forward.
  pub fn adopt(&mut self, configuration: RootConfiguration) -> bool {
    if configuration.version <= self.configuration.version {
      return false;
    }
    self.configuration = configuration;
    true
  }

  /// Applies every committed but not-yet-applied command to the root configuration, in commit order — the
  /// deterministic fold every voter makes, so the configuration is the same on all of them. The fold starts
  /// from the formed base (a node's first fold replaces an adopted configuration with it), so a host
  /// promoted to voter applies the log exactly once from the same starting point as every other voter.
  fn apply_committed(&mut self) {
    let committed = self.raft.committed_entries().to_vec();
    if self.applied == 0 && !committed.is_empty() {
      self.configuration = self.base.clone();
    }
    while let Some(entry) = committed.get(usize::try_from(self.applied).unwrap_or(usize::MAX)) {
      if let Some(command) = RootCommand::decode(&entry.command) {
        self.apply_root(command);
      }
      self.applied = self.applied.saturating_add(1);
    }
  }

  /// Applies one committed command to the root configuration.
  fn apply_root(&mut self, command: RootCommand) {
    match command {
      RootCommand::AdmitRegion(region) => {
        self.configuration.admit_region(region);
      }
      RootCommand::RetireRegion(region) => {
        self.configuration.retire_region(region);
      }
      RootCommand::PromoteRegion { lost, mirror } => {
        self.configuration.promote_region(lost, mirror);
      }
      RootCommand::MoveHome { volume, to } => {
        self.configuration.move_home(volume, to);
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const P0: HostId = HostId(1);
  const P1: HostId = HostId(2);
  const R0: RegionId = RegionId(0);
  const R1: RegionId = RegionId(1);
  const R2: RegionId = RegionId(2);
  const P2: HostId = HostId(3);

  /// A root group per representative host: the regions `[R0, R1, R2]` with `[P0, P1, P2]` as their
  /// representatives and voters.
  fn groups() -> BTreeMap<HostId, RootGroup> {
    [P0, P1, P2]
      .into_iter()
      .map(|node| {
        (
          node,
          RootGroup::new(node, vec![R0, R1, R2], vec![P0, P1, P2]),
        )
      })
      .collect()
  }

  /// Elects `candidate` with the votes of the `reachable` peers (pre-vote then vote), broadcasting each
  /// request and folding every reply until the exchange settles.
  fn elect_among(
    groups: &mut BTreeMap<HostId, RootGroup>,
    candidate: HostId,
    reachable: &[HostId],
  ) {
    let mut pending = groups
      .get_mut(&candidate)
      .map(RootGroup::election_timeout)
      .unwrap_or_default();
    while let Some(request) = pending.pop() {
      pending.clear();
      for &peer in reachable.iter().filter(|peer| **peer != candidate) {
        let reply = groups
          .get_mut(&peer)
          .and_then(|group| group.answer(request.clone()));
        if let Some(reply) = reply
          && let Some(group) = groups.get_mut(&candidate)
        {
          pending.extend(group.fold_reply(reply));
        }
      }
    }
  }

  /// One replication round from `leader` to each of its targets that is `reachable`.
  fn replicate_round(
    groups: &mut BTreeMap<HostId, RootGroup>,
    leader: HostId,
    reachable: &[HostId],
  ) {
    let targets: Vec<HostId> = groups
      .get(&leader)
      .map(RootGroup::voters)
      .unwrap_or_default()
      .into_iter()
      .filter(|peer| *peer != leader && reachable.contains(peer))
      .collect();
    for peer in targets {
      let Some(append) = groups.get(&leader).and_then(|l| l.replication_for(peer)) else {
        continue;
      };
      let reply = groups
        .get_mut(&peer)
        .and_then(|group| group.answer(RaftMessage::AppendEntries(append)));
      if let Some(reply) = reply
        && let Some(group) = groups.get_mut(&leader)
      {
        group.fold_reply(reply);
      }
    }
  }

  /// AC (§4.8, D-14; Raft §6): a retired region's **representative leaves the root voter set** — it stops
  /// counting toward every root majority — and the surviving representatives commit alone. Three regions
  /// with representatives {P0, P1, P2}; region R2's hosts die; the leader retires R2 (committed by P0 and
  /// P1), then moves the voter set to the representatives of the committed regions: the joint change and
  /// `C_new` each commit with P0 and P1; afterwards `is_voter(P2)` is false on both survivors and a further
  /// root change commits with P1's acknowledgement alone. Before this the root voter set was fixed at boot
  /// and the dead representative counted toward every majority for good.
  #[test]
  fn a_retired_regions_representative_leaves_the_root_voters() {
    let mut groups = groups();
    elect_among(&mut groups, P0, &[P0, P1, P2]);
    assert!(groups[&P0].is_leader());

    // Region R2's hosts die: the leader retires the region (no mirror), committed by the survivors. The
    // retirement alone leaves the voter set untouched — the voter change follows.
    let survivors = [P0, P1];
    let proposed = groups
      .get_mut(&P0)
      .is_some_and(|leader| leader.reconcile_regions(&[R0, R1], &BTreeMap::new()));
    replicate_round(&mut groups, P0, &survivors);
    replicate_round(&mut groups, P0, &survivors);
    assert_eq!(
      (
        proposed,
        groups[&P0].configuration().regions.contains(&R2),
        groups[&P0].is_voter(P2),
      ),
      (true, false, true),
      "the retirement was proposed, committed and dropped R2; P2 still votes until the voter change"
    );

    // The voter set follows the committed regions' representatives: the joint change, then C_new, each
    // committed by the survivors; then nothing more.
    let representatives: BTreeMap<RegionId, HostId> = [(R0, P0), (R1, P1)].into_iter().collect();
    let mut steps = Vec::new();
    for rounds in [1, 2, 0] {
      steps.push(
        groups
          .get_mut(&P0)
          .is_some_and(|leader| leader.reconcile_voters(&representatives)),
      );
      for _ in 0..rounds {
        replicate_round(&mut groups, P0, &survivors);
      }
    }
    assert_eq!(
      steps,
      vec![true, true, false],
      "began the joint change, completed it once committed, then settled"
    );
    assert_eq!(
      (
        groups[&P0].is_voter(P2),
        groups[&P1].is_voter(P2),
        groups[&P0].voters(),
      ),
      (false, false, vec![P0, P1]),
      "P2 left the root voter set on both survivors"
    );

    // A further root change commits with P1's acknowledgement alone.
    let admitted = RegionId(3);
    let proposed = groups
      .get_mut(&P0)
      .is_some_and(|leader| leader.propose(RootCommand::AdmitRegion(admitted)));
    replicate_round(&mut groups, P0, &survivors);
    replicate_round(&mut groups, P0, &survivors);
    assert_eq!(
      (
        proposed,
        groups[&P0].configuration().regions.contains(&admitted),
        groups[&P1].configuration().regions.contains(&admitted),
      ),
      (true, true, true),
      "the surviving representatives commit and apply a further change alone"
    );
  }

  /// Each root command round-trips through encode/decode, and a malformed entry decodes to `None` (applied as
  /// a safe no-op rather than panicking).
  #[test]
  fn root_command_round_trips() {
    let commands = [
      RootCommand::AdmitRegion(R2),
      RootCommand::RetireRegion(R1),
      RootCommand::PromoteRegion {
        lost: R1,
        mirror: R0,
      },
      RootCommand::MoveHome {
        volume: ObjectId::new(P0, 7),
        to: R1,
      },
    ];
    for command in commands {
      assert_eq!(
        RootCommand::decode(&command.encode()),
        Some(command),
        "round-trip is identity"
      );
    }
    assert_eq!(
      RootCommand::decode(&[]),
      None,
      "empty bytes decode to nothing"
    );
    assert_eq!(
      RootCommand::decode(&[9, 9, 9]),
      None,
      "an unknown tag decodes to nothing"
    );
  }

  /// Sends one request from `from` to `to`: `to` answers it (serve) and `from` folds the answer (drive),
  /// returning the follow-on messages `from` must send next (the pre-vote → vote transition emits the real
  /// vote requests). In process; the transport-carried form is the daemon's (owed).
  fn exchange(from: &mut RootGroup, to: &mut RootGroup, request: RaftMessage) -> Vec<RaftMessage> {
    match to.answer(request) {
      Some(reply) => from.fold_reply(reply),
      None => Vec::new(),
    }
  }

  /// A two-voter root group on `node`, holding `[R0, R1]` as its initial regions and `[P0, P1]` as voters.
  fn group(node: HostId) -> RootGroup {
    RootGroup::new(node, vec![R0, R1], vec![P0, P1])
  }

  /// Elects `leader` over `follower` through the full pre-vote then real-vote round (§9.6), draining every
  /// follow-on message until the exchange settles — the in-process form of the transport election.
  fn elect(leader: &mut RootGroup, follower: &mut RootGroup) {
    let mut pending = leader.election_timeout();
    while let Some(request) = pending.pop() {
      pending.extend(exchange(leader, follower, request));
    }
  }

  /// Runs `rounds` replication rounds from `leader` to `follower` (round one replicates the entry and the
  /// leader commits at the majority; round two's heartbeat carries the advanced commit index, so the follower
  /// applies too).
  fn replicate(leader: &mut RootGroup, follower: &mut RootGroup, rounds: usize) {
    for _ in 0..rounds {
      if let Some(append) = leader.replication_for(P1) {
        exchange(leader, follower, RaftMessage::AppendEntries(append));
      }
    }
  }

  /// AC (§4.8, D-14 — Raft Figure 2's follower timer-resets, the root group's parallel of the council's):
  /// **granting a real vote** advances the voter's leader-contact signal, so the drive loop's election timer
  /// resets and the voter defers its own campaign a full timeout — the candidate it backed gets time to win
  /// and send its first heartbeat (docs/bugs/2026-09-13-consensus-voters-outside-record-neighbourhood.md, sibling
  /// fixes). By use: after the election the follower has granted a real vote but answered no append yet, and
  /// its contact has advanced; the leader's first heartbeat then advances it again.
  #[test]
  fn granting_a_root_vote_defers_the_voters_own_election() {
    let mut leader = group(P0);
    let mut follower = group(P1);
    assert_eq!(
      follower.leader_contact(),
      0,
      "no contact before any election"
    );
    elect(&mut leader, &mut follower);
    assert!(leader.is_leader());
    let after_vote = follower.leader_contact();
    assert!(
      after_vote > 0,
      "granting the real vote advanced the follower's contact — the drive loop defers its campaign"
    );
    replicate(&mut leader, &mut follower, 1);
    assert!(
      follower.leader_contact() > after_vote,
      "the leader's first heartbeat advances it again"
    );
  }

  /// AC (§4.8, D-14 — Raft Figure 2's follower timer-resets, the root group's parallel of the council's): an
  /// append from the **current** root leader resets the election timer **even when the log-consistency check
  /// rejects it** — a follower still catching up has a live leader and must not campaign against it. A
  /// stale-term append is not contact and does not reset.
  #[test]
  fn a_current_root_leaders_rejected_append_still_resets_the_election_timer() {
    let mut leader = group(P0);
    let mut follower = group(P1);
    elect(&mut leader, &mut follower);
    let before = follower.leader_contact();
    let mut rejected = leader
      .replication_for(P1)
      .expect("the leader owes its follower a heartbeat");
    rejected.prev_log_index = rejected.prev_log_index.saturating_add(10);
    rejected.prev_log_term = rejected.prev_log_term.saturating_add(1);
    let reply = follower.answer(RaftMessage::AppendEntries(rejected));
    assert!(
      matches!(reply, Some(RaftMessage::AppendReply(ref r)) if !r.success),
      "the append was rejected by the log-consistency check"
    );
    let after_rejected = follower.leader_contact();
    assert!(
      after_rejected > before,
      "a rejected current-term append is still leader contact — the election timer resets"
    );
    let mut stale = leader
      .replication_for(P1)
      .expect("the leader owes its follower a heartbeat");
    stale.term = 0;
    let reply = follower.answer(RaftMessage::AppendEntries(stale));
    assert!(
      matches!(reply, Some(RaftMessage::AppendReply(ref r)) if !r.success),
      "a stale-term append is refused"
    );
    assert_eq!(
      follower.leader_contact(),
      after_rejected,
      "and it is not contact — the timer does not reset for a deposed leader"
    );
  }

  /// AC (§4.8, D-14 — Raft §6.2 CheckQuorum on the root group, the parallel of the council's): a root leader
  /// that hears from no majority across a whole window steps down; one whose voters keep acknowledging stays.
  #[test]
  fn a_root_leader_that_hears_from_no_majority_steps_down_at_its_check_quorum_tick() {
    let mut leader = group(P0);
    let mut follower = group(P1);
    elect(&mut leader, &mut follower);
    assert!(leader.is_leader());
    leader.check_quorum();
    assert!(
      leader.is_leader(),
      "the first window still counts the majority that elected it"
    );
    replicate(&mut leader, &mut follower, 1);
    leader.check_quorum();
    assert!(
      leader.is_leader(),
      "a voter's acknowledgement this window keeps it leader"
    );
    leader.check_quorum();
    assert!(
      !leader.is_leader(),
      "a whole window with no acknowledgement steps it down"
    );
  }

  /// AC (§4.8, D-14): the root group elects a leader and commits a **region-membership** change at a majority,
  /// applying it to the root configuration on every voter — the distributed root master. The drive rides the
  /// same Raft the transport carries.
  #[test]
  fn a_root_group_commits_a_region_membership_change_at_a_majority() {
    let mut leader = group(P0);
    let mut follower = group(P1);

    elect(&mut leader, &mut follower);
    assert!(
      leader.is_leader(),
      "the leader won the pre-vote then the real vote"
    );

    // A non-leader may not propose; the leader admits a new region, which commits only at the majority.
    assert!(
      !follower.propose(RootCommand::AdmitRegion(R2)),
      "a non-leader proposes nothing"
    );
    assert!(
      leader.propose(RootCommand::AdmitRegion(R2)),
      "the leader appended the region-membership change"
    );
    replicate(&mut leader, &mut follower, 2);
    assert!(
      leader.configuration().regions.contains(&R2),
      "the region-membership change committed and applied at the leader"
    );
    assert!(
      follower.configuration().regions.contains(&R2),
      "and at the follower — the root group agrees on region membership"
    );
  }

  /// AC (§4.8 "region loss promotes the mirror through the root group"): the leader commits a **promotion** of
  /// a lost region to its mirror at a majority; every voter drops the lost region and routes a volume homed
  /// there to the mirror. This is the cross-region takeover the root group exists to decide.
  #[test]
  fn a_root_group_commits_a_region_promotion_at_a_majority() {
    let mut leader = group(P0);
    let mut follower = group(P1);
    elect(&mut leader, &mut follower);
    assert!(leader.is_leader());

    let volume = ObjectId::new(P0, 7); // created in region R0
    assert!(
      leader.propose(RootCommand::PromoteRegion {
        lost: R0,
        mirror: R1,
      }),
      "the leader proposes promoting the lost region's mirror"
    );
    replicate(&mut leader, &mut follower, 2);

    for group in [&leader, &follower] {
      assert!(
        !group.configuration().regions.contains(&R0),
        "the lost region is dropped from the membership on every voter"
      );
      assert_eq!(
        group.configuration().home_of(volume, R0),
        R1,
        "a volume homed in the lost region is served from its promoted mirror"
      );
    }
  }

  /// AC (§4.8, D-14): the leader reconciles the region membership from the alive set — a newly-alive region is
  /// admitted, a region no host is alive in is retired, each committed at the majority; a non-leader proposes
  /// nothing, and a change still in flight is not re-proposed (the caught-up gate).
  #[test]
  fn the_leader_reconciles_region_membership_from_the_alive_set() {
    let mut leader = group(P0); // regions [R0, R1]
    let mut follower = group(P1);
    elect(&mut leader, &mut follower);
    assert!(leader.is_leader());

    // R2 has become alive; R1 has lost every host and has **no mirror**, so it is retired. A non-leader
    // proposes nothing.
    let alive = [R0, R2];
    let no_mirrors = std::collections::BTreeMap::new();
    assert!(
      !follower.reconcile_regions(&alive, &no_mirrors),
      "a non-leader proposes nothing"
    );
    assert!(
      leader.reconcile_regions(&alive, &no_mirrors),
      "the leader proposes admitting R2 and retiring the mirror-less lost R1"
    );
    assert!(
      !leader.reconcile_regions(&alive, &no_mirrors),
      "a change in flight is not re-proposed (the caught-up gate)"
    );

    replicate(&mut leader, &mut follower, 2);
    for g in [&leader, &follower] {
      assert!(g.configuration().regions.contains(&R2), "R2 was admitted");
      assert!(
        !g.configuration().regions.contains(&R1),
        "the mirror-less lost R1 was retired"
      );
    }
    assert!(
      !leader.reconcile_regions(&alive, &no_mirrors),
      "the settled membership reconciles to a no-op — the log grows only for real changes"
    );
  }

  /// AC (§4.8 — region-loss promotion at operator cadence): a lost region that has a **mirror** is left in
  /// the membership (it awaits a deliberate operator promotion, not an automatic failover of a possibly-just-
  /// partitioned region — split-brain), whereas a mirror-less lost region is retired.
  #[test]
  fn a_lost_mirrored_region_is_not_auto_retired() {
    let mut leader = group(P0); // regions [R0, R1]
    let mut follower = group(P1);
    elect(&mut leader, &mut follower);
    assert!(leader.is_leader());

    // R1 is lost. With R0 declared as its mirror, the reconcile does not retire it.
    let mirrors = std::collections::BTreeMap::from([(R1, R0)]);
    assert!(
      !leader.reconcile_regions(&[R0], &mirrors),
      "a lost region with a mirror is not auto-retired — cross-region failover is the operator's"
    );
    assert!(
      leader.configuration().regions.contains(&R1),
      "the mirrored lost region stays in the membership, awaiting an operator promotion"
    );

    // The same region with no mirror declared is retired on its loss (no failover target).
    assert!(
      leader.reconcile_regions(&[R0], &std::collections::BTreeMap::new()),
      "a mirror-less lost region is retired"
    );
    replicate(&mut leader, &mut follower, 2);
    assert!(!leader.configuration().regions.contains(&R1));
  }
}
