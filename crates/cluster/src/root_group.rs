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

/// The **root configuration group** on one node (§4.8, D-14 — the root group across regions): the multi-voter
/// [`RaftNode`] over the root-group hosts, producing the [`RootConfiguration`] every region learns. A change
/// commits at a majority over the (cross-region) transport — the leader [`propose`](RootGroup::propose)s, the
/// followers serve, and each committed command applies to the root configuration. The drive primitives ride
/// the same [`RaftMessage`] wire the transport carries, exactly as the regional council's do.
pub struct RootGroup {
  raft: RaftNode,
  configuration: RootConfiguration,
  applied: u64,
  /// Monotone count of appends this node has answered from a leader — the drive loop's election timer reads
  /// it: while it advances, a leader is alive and this node does not campaign; once it stalls for the election
  /// timeout, the leader is presumed gone and a pre-election begins.
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
    RootGroup {
      raft,
      configuration: RootConfiguration::formed(regions),
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

  /// The group's voter set — the small consensus group the drive loop ships elections and replication to.
  pub fn voters(&self) -> Vec<HostId> {
    self.raft.all_voters()
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

  /// **SERVE**: answers a request received over the transport — a pre-vote, a vote request, or an append —
  /// returning the reply to ship back and applying whatever newly committed to the root configuration (a
  /// follower applies on the append). A reply is not a request and is not answered here — its sender folds it
  /// with [`fold_reply`](RootGroup::fold_reply).
  pub fn answer(&mut self, request: RaftMessage) -> Option<RaftMessage> {
    match request {
      RaftMessage::PreVote(pre) => Some(RaftMessage::PreVoteReply(self.raft.on_pre_vote(pre))),
      RaftMessage::RequestVote(vote) => {
        Some(RaftMessage::VoteReply(self.raft.on_request_vote(vote)))
      }
      RaftMessage::AppendEntries(append) => {
        let reply = self.raft.on_append_entries(append);
        if reply.success {
          // A valid append from the current leader is a heartbeat — reset the election timer.
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
  /// proposes admitting every alive region not yet a member and retiring every member region no host is alive
  /// in, each through the root log so it commits at a majority over the transport and applies on every voter.
  /// Returns whether anything was proposed. A non-leader proposes nothing (only the elected root master
  /// decides), and the [`caught_up`](RootGroup::caught_up) gate keeps a change in flight from being
  /// re-proposed, so the log grows only for real changes. A lost region is **retired** here (the region-loss
  /// **promotion** to a configured mirror — §4.8 "region loss promotes the mirror through the root group" — is
  /// the owed refinement; without a mirror declared, a region with no live host simply leaves the membership).
  pub fn reconcile_regions(&mut self, alive: &[RegionId]) -> bool {
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
      proposed |= self.propose(RootCommand::RetireRegion(region));
    }
    proposed
  }

  /// Whether `node` is a **voter** of this root group — a host of its Raft consensus set. A non-voter learns
  /// the committed root configuration by fetching it and [`adopt`](RootGroup::adopt)ing it.
  pub fn is_voter(&self, node: HostId) -> bool {
    self.raft.all_voters().contains(&node)
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
  /// deterministic fold every voter makes, so the configuration is the same on all of them.
  fn apply_committed(&mut self) {
    let committed = self.raft.committed_entries().to_vec();
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

    // R2 has become alive; R1 has lost every host. A non-leader proposes nothing.
    let alive = [R0, R2];
    assert!(
      !follower.reconcile_regions(&alive),
      "a non-leader proposes nothing"
    );
    assert!(
      leader.reconcile_regions(&alive),
      "the leader proposes admitting R2 and retiring R1"
    );
    assert!(
      !leader.reconcile_regions(&alive),
      "a change in flight is not re-proposed (the caught-up gate)"
    );

    replicate(&mut leader, &mut follower, 2);
    for g in [&leader, &follower] {
      assert!(g.configuration().regions.contains(&R2), "R2 was admitted");
      assert!(!g.configuration().regions.contains(&R1), "R1 was retired");
    }
    assert!(
      !leader.reconcile_regions(&alive),
      "the settled membership reconciles to a no-op — the log grows only for real changes"
    );
  }
}
