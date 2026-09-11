//! The regional configuration council (§4.8 "Configuration, by consensus", D-14 — the "configuration
//! master", a small elected council per region) — the authority that maintains the
//! `RegionalConfiguration` every register request carries: the region's membership, each owner's bounded
//! neighbourhood, the per-host fencing epochs, and the fault tolerance. It is touched **only** on
//! membership, takeover, neighbourhood and home changes — never on a per-write path (banned item 10) — and
//! read per request from a local copy.
//!
//! The configuration is the deterministic fold of a **committed Raft log** ([`crate::raft`], the hecate
//! dialect): each change — admit, retire, takeover — is a [`ConfigCommand`] proposed to the log by the
//! leader and applied only once committed, so every voter reaches the same configuration, and the version
//! bumps once per applied change so a request under the stale version is refused (`ConfigurationStale`).
//! Degenerate on a laptop (`f = 0`): the sole voter self-elects and its append commits at once, so a change
//! applies synchronously — the identical code path a fleet runs through replication, never a mode switch
//! (R8).
//!
//! [`RegionalCouncil`] is one node's participant in that consensus. It runs the Raft **live over the fleet
//! transport** — the daemon's record-plane coordinator (`slates_server::fleet`) drives its election and
//! replication and installs its committed configuration into placement — so this module stays sans-io and
//! the drive lives there. A **small** elected voter set carries the consensus; the wider region's members
//! that do not vote are **learners** that fetch the committed configuration (`is_voter`/`adopt`). Taking
//! over a failed host bumps that host's fencing epoch ([`ConfigCommand::TakeOver`]), so a resumed stale
//! owner is refused `StaleEpoch`; the phase-one recovery and adoption the new owner then runs live in
//! `slates_db::register` (`install_authority`/`prepare`) and [`crate`]
//! (`promote_record`/`promote_under_configuration`), oracle-tested for Continuity and StaleNeverCommits.
//! Owed: joint consensus to change the voter set; the FencedRegister TLA+ revalidation for the per-host
//! epoch fence (A-9, §4.8 lines 1710-1712).

use std::mem::size_of;

use slates_db::register::{DomainId, HostId, Quorum, RegionalConfiguration};

use crate::raft::{AppendEntries, RaftNode};
use crate::raft_wire::RaftMessage;

/// A configuration change as it rides the Raft log — the command a committed [`LogEntry`](crate::raft::LogEntry)
/// carries, decoded and applied to the [`Configuration`] in commit order so every voter reaches the same
/// configuration. The Raft core treats it as opaque bytes; this is the config group's interpretation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigCommand {
  /// Admit a member to the neighbourhood.
  Admit(HostId),
  /// Retire a member from the neighbourhood.
  Retire(HostId),
  /// Take over a **failed host** (§4.8 line 1730 "Host failure increments the host epoch"): bump its
  /// fencing epoch and retire it, so a resumed stale owner's records under the old epoch are refused
  /// `StaleEpoch`. Per host, not per object — one bump fences every object the host owned; the surviving
  /// owner of each object is recomputed by rendezvous over the new neighbourhood, not named here.
  TakeOver {
    /// The failed host being taken over.
    dead: HostId,
  },
}

/// Format: a config command is a one-byte tag followed by its little-endian fields; these are the tags.
const COMMAND_ADMIT: u8 = 0;
const COMMAND_RETIRE: u8 = 1;
const COMMAND_TAKE_OVER: u8 = 2;

impl ConfigCommand {
  /// The command's canonical bytes for the log: the tag, then the host id (and, for a takeover, the
  /// object), little-endian.
  pub fn encode(&self) -> Vec<u8> {
    let mut out = Vec::new();
    match self {
      ConfigCommand::Admit(host) => {
        out.push(COMMAND_ADMIT);
        out.extend_from_slice(&host.0.to_le_bytes());
      }
      ConfigCommand::Retire(host) => {
        out.push(COMMAND_RETIRE);
        out.extend_from_slice(&host.0.to_le_bytes());
      }
      ConfigCommand::TakeOver { dead } => {
        out.push(COMMAND_TAKE_OVER);
        out.extend_from_slice(&dead.0.to_le_bytes());
      }
    }
    out
  }

  /// Decodes a command from a committed log entry, or `None` if the bytes are malformed (a corrupt log
  /// entry — never expected from our own [`encode`](ConfigCommand::encode), applied as a no-op if seen).
  pub fn decode(bytes: &[u8]) -> Option<ConfigCommand> {
    let (&tag, rest) = bytes.split_first()?;
    match tag {
      COMMAND_ADMIT => Some(ConfigCommand::Admit(take_host(rest)?.0)),
      COMMAND_RETIRE => Some(ConfigCommand::Retire(take_host(rest)?.0)),
      COMMAND_TAKE_OVER => Some(ConfigCommand::TakeOver {
        dead: take_host(rest)?.0,
      }),
      _ => None,
    }
  }
}

/// Reads a u64 host id at the front of `bytes`, returning it and the remainder, or `None` if truncated.
fn take_host(bytes: &[u8]) -> Option<(HostId, &[u8])> {
  if bytes.len() < size_of::<u64>() {
    return None;
  }
  let (head, rest) = bytes.split_at(size_of::<u64>());
  let mut word = [0u8; size_of::<u64>()];
  word.copy_from_slice(head);
  Some((HostId(u64::from_le_bytes(word)), rest))
}

/// A proposed change to the configuration — the vocabulary the SWIM view and takeover speak to the
/// group. Applied locally at `f = 0`; carried through consensus at `f > 0` (owed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reconfiguration {
  /// Admit a member to the neighbourhood (a join the membership view learned).
  Admit(HostId),
  /// Retire a member from the neighbourhood (a clean departure — no fencing epoch bump).
  Retire(HostId),
  /// Take over a **failed** member (§4.8 "Promotion and takeover", line 1730 "Host failure increments the
  /// host epoch"): bump its fencing epoch **and** retire it, so a resumed stale owner's records under the
  /// old epoch are refused `StaleEpoch`. This is what the leader proposes for a SWIM-confirmed death, the
  /// per-host counterpart of a clean [`Retire`](Reconfiguration::Retire).
  TakeOver(HostId),
}

/// The **regional configuration council** on one node (§4.8, D-14 — the "configuration master", a small
/// elected council per region): the multi-voter [`RaftNode`] over the council voters, producing the
/// [`RegionalConfiguration`] every node learns. Membership changes commit at a majority over the transport
/// — the leader [`propose`](RegionalCouncil::propose)s, the followers serve, and each committed command
/// applies to the regional configuration. The drive primitives ride the same [`RaftMessage`] wire the fleet
/// transport carries.
pub struct RegionalCouncil {
  raft: RaftNode,
  configuration: RegionalConfiguration,
  applied: u64,
  scatter: u64,
  /// Monotone count of appends this node has answered from a leader — the drive loop's election timer reads
  /// it: while it advances, a leader is alive and this node does not campaign; once it stalls for the
  /// election timeout, the leader is presumed gone and a pre-election begins.
  leader_contact: u64,
}

impl RegionalCouncil {
  /// A council on this `node` (one of the `voters`) holding the region's `members` at `quorum` and the
  /// failure `domains`, each neighbourhood bounded to `scatter`. A multi-voter council waits for an election
  /// over the transport (the drive loop); a **sole voter** self-elects at once (the `f = 0` laptop
  /// degenerate — the same council code a fleet runs, immediately its own leader so it may reconcile and
  /// propose with no messages, R8). The configuration starts at the formed region.
  pub fn new(
    node: HostId,
    members: Vec<HostId>,
    voters: Vec<HostId>,
    quorum: Quorum,
    domains: std::collections::BTreeMap<HostId, DomainId>,
    scatter: u64,
    has_mirror: bool,
  ) -> RegionalCouncil {
    let mut raft = RaftNode::new(node, voters);
    // A council of one is immediately its own majority: elect at once so the laptop's council is the
    // authority with no drive loop (there is no fleet transport at `f = 0`). A multi-voter council does
    // not self-elect — it must win a real election over the transport.
    if raft.all_voters().len() == 1 {
      let _ = raft.start_election();
    }
    let configuration =
      RegionalConfiguration::formed(members, quorum, domains, scatter, has_mirror);
    RegionalCouncil {
      raft,
      configuration,
      applied: 0,
      scatter,
      leader_contact: 0,
    }
  }

  /// The regional configuration the council has agreed on so far — every node's placement view derives from
  /// it ([`RegionalConfiguration::configuration_for`]).
  pub fn configuration(&self) -> &RegionalConfiguration {
    &self.configuration
  }

  /// Whether this node leads the council (only the leader may propose).
  pub fn is_leader(&self) -> bool {
    self.raft.is_leader()
  }

  /// The council's voter set — the small consensus group the drive loop ships elections and replication to.
  pub fn voters(&self) -> Vec<HostId> {
    self.raft.all_voters()
  }

  /// The leader-contact count (see the field): the drive loop's election timer resets while this advances.
  pub fn leader_contact(&self) -> u64 {
    self.leader_contact
  }

  /// **DRIVE**: begins a **pre-election** on an election timeout (Raft §9.6), returning the [`PreVote`]s to
  /// ship to the other voters — asked *without inflating the term*, so a partitioned node cannot disrupt a
  /// healthy leader. A lone voter proceeds straight to leading with no messages (the `f = 0` degenerate).
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
  /// returning the reply to ship back and applying whatever newly committed to the regional configuration
  /// (a follower applies on the append). A reply (`VoteReply`/`PreVoteReply`/`AppendReply`) is not a request
  /// and is not answered here — its sender folds it with [`fold_reply`](RegionalCouncil::fold_reply).
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
  /// regional configuration on an append reply (the leader once a majority acknowledges).
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

  /// Proposes a membership change on the leader **without waiting**: it commits — and applies to the
  /// regional configuration — only once a majority of the council acknowledge it over the transport. Returns
  /// whether the leader appended it (a non-leader, or a change already reflected in the membership, returns
  /// `false`).
  pub fn propose(&mut self, change: Reconfiguration) -> bool {
    let (command, would_change) = match change {
      Reconfiguration::Admit(host) => (
        ConfigCommand::Admit(host),
        !self.configuration.members.contains(&host),
      ),
      Reconfiguration::Retire(host) => (
        ConfigCommand::Retire(host),
        self.configuration.members.contains(&host),
      ),
      Reconfiguration::TakeOver(host) => (
        // Per-host takeover: bump the host's fencing epoch and retire it (one bump fences every object it
        // owned; the surviving owner of each is recomputed by rendezvous, not named here).
        ConfigCommand::TakeOver { dead: host },
        self.configuration.members.contains(&host),
      ),
    };
    if !would_change {
      return false;
    }
    self.raft.append_command(command.encode())
  }

  /// Whether the council's log is fully committed — nothing proposed is still in flight
  /// (`last_log_index == commit_index`). [`reconcile_alive`](RegionalCouncil::reconcile_alive) reads it so
  /// a change proposed but not yet committed is **not re-proposed** each period: unlike a solo `f = 0`
  /// council whose append commits at once, the council's [`propose`](RegionalCouncil::propose) commits only
  /// later over the transport, so `configuration.members` does not reflect the change until then, and an
  /// ungated reconcile
  /// would append a duplicate command every period until the first commits (the apply step would no-op them,
  /// but the log would bloat against the near-zero commit rate the design makes a tripwire). Serialises
  /// reconfiguration to one batch per commit cycle, which the rare configuration change can afford.
  pub fn caught_up(&self) -> bool {
    self.raft.last_log_index() == self.raft.commit_index()
  }

  /// Reconciles the regional membership with a SWIM `alive` set **as the leader** (§4.8 "the configuration
  /// master decides membership"): proposes admitting every alive host not yet a member and retiring every
  /// member no longer alive, each through the council log so it commits at a majority over the transport and
  /// applies on every voter. Returns whether anything was proposed. A non-leader proposes nothing — the
  /// leader is the one configuration master and decides from its own SWIM view (it probes every member), so
  /// a follower's own detection need not propose. Gated on [`caught_up`](RegionalCouncil::caught_up) so a
  /// change in flight is not re-proposed; a member already present, or already gone, is not proposed either
  /// (`propose`'s own `would_change` gate), so the log grows only for real changes.
  pub fn reconcile_alive(&mut self, alive: &[HostId]) -> bool {
    if !self.is_leader() || !self.caught_up() {
      return false;
    }
    let mut proposed = false;
    for host in alive {
      proposed |= self.propose(Reconfiguration::Admit(*host));
    }
    let stale: Vec<HostId> = self
      .configuration
      .members
      .iter()
      .copied()
      .filter(|member| !alive.contains(member))
      .collect();
    for host in stale {
      // A member no longer alive **failed** (SWIM confirmed its death), so take it over — bump its fencing
      // epoch and retire it (§4.8 line 1730 "Host failure increments the host epoch"), not a clean retire.
      proposed |= self.propose(Reconfiguration::TakeOver(host));
    }
    proposed
  }

  /// Whether `node` is a **voter** of this council — a member of the Raft consensus set. A non-voter
  /// (a **learner**) does not vote; it learns the committed configuration by fetching it from a voter and
  /// [`adopt`](RegionalCouncil::adopt)ing it. The drive loop reads this to take the voter path (drive the
  /// Raft) or the learner path (fetch).
  pub fn is_voter(&self, node: HostId) -> bool {
    self.raft.all_voters().contains(&node)
  }

  /// Adopts a configuration a **learner** fetched from a council voter (§4.8, D-14: the council is a small
  /// elected set, so a non-voter member learns the committed configuration rather than voting on it).
  /// Returns whether it advanced — a fetch that is not newer than what this node already has (it is current,
  /// or the fetch raced a newer local view) is ignored, so adoption only moves forward. Only the drive
  /// loop's learner branch calls this; a voter's configuration is the deterministic fold of its Raft log.
  pub fn adopt(&mut self, configuration: RegionalConfiguration) -> bool {
    if configuration.version <= self.configuration.version {
      return false;
    }
    self.configuration = configuration;
    true
  }

  /// Applies every committed but not-yet-applied command to the regional configuration, in commit order —
  /// the deterministic fold every voter makes, so the configuration is the same on all of them.
  fn apply_committed(&mut self) {
    let committed = self.raft.committed_entries().to_vec();
    while let Some(entry) = committed.get(usize::try_from(self.applied).unwrap_or(usize::MAX)) {
      if let Some(command) = ConfigCommand::decode(&entry.command) {
        self.apply_regional(command);
      }
      self.applied = self.applied.saturating_add(1);
    }
  }

  /// Applies one committed command to the regional configuration.
  fn apply_regional(&mut self, command: ConfigCommand) {
    match command {
      ConfigCommand::Admit(host) => {
        self.configuration.admit(host, self.scatter);
      }
      ConfigCommand::Retire(host) => {
        self.configuration.retire(host, self.scatter);
      }
      ConfigCommand::TakeOver { dead } => {
        self.configuration.take_over(dead, self.scatter);
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const OWNER: HostId = HostId(1);
  const A: HostId = HostId(2);
  const B: HostId = HostId(3);

  /// Each config command round-trips through encode/decode, and a malformed entry decodes to `None`
  /// (applied as a safe no-op rather than panicking).
  #[test]
  fn config_command_round_trips() {
    let commands = [
      ConfigCommand::Admit(A),
      ConfigCommand::Retire(B),
      ConfigCommand::TakeOver { dead: OWNER },
    ];
    for command in commands {
      assert_eq!(
        ConfigCommand::decode(&command.encode()),
        Some(command),
        "round-trip is identity"
      );
    }
    assert_eq!(
      ConfigCommand::decode(&[]),
      None,
      "empty bytes decode to nothing"
    );
    assert_eq!(
      ConfigCommand::decode(&[9, 9, 9]),
      None,
      "an unknown tag decodes to nothing"
    );
  }

  /// Sends one request from `from` to `to`: `to` answers it (serve) and `from` folds the answer (drive),
  /// returning the follow-on messages `from` must send next (the pre-vote → vote transition emits the real
  /// vote requests). In process; the transport-carried form is proven in `config_group_live`.
  fn exchange(
    from: &mut RegionalCouncil,
    to: &mut RegionalCouncil,
    request: RaftMessage,
  ) -> Vec<RaftMessage> {
    match to.answer(request) {
      Some(reply) => from.fold_reply(reply),
      None => Vec::new(),
    }
  }

  /// A two-voter council on `node`, holding `[OWNER, A]` as its members and voters at f=1.
  fn council(node: HostId) -> RegionalCouncil {
    RegionalCouncil::new(
      node,
      vec![OWNER, A],
      vec![OWNER, A],
      Quorum { f: 1 },
      std::collections::BTreeMap::new(),
      3,
      false,
    )
  }

  /// Elects `leader` over `follower` through the full pre-vote then real-vote round (§9.6), draining every
  /// follow-on message until the exchange settles — the in-process form of the transport election.
  fn elect(leader: &mut RegionalCouncil, follower: &mut RegionalCouncil) {
    let mut pending = leader.election_timeout();
    while let Some(request) = pending.pop() {
      pending.extend(exchange(leader, follower, request));
    }
  }

  /// Runs `rounds` replication rounds from `leader` to `follower` (round one replicates the entry and the
  /// leader commits at the majority; round two's heartbeat carries the advanced commit index, so the
  /// follower applies too).
  fn replicate(leader: &mut RegionalCouncil, follower: &mut RegionalCouncil, rounds: usize) {
    for _ in 0..rounds {
      if let Some(append) = leader.replication_for(A) {
        exchange(leader, follower, RaftMessage::AppendEntries(append));
      }
    }
  }

  /// AC (§4.8, D-14): the regional council elects a leader and commits a membership change at a majority,
  /// applying it to the regional configuration on every voter — the distributed configuration master. A
  /// member admitted by the council need not be a voter of it (the council is small; the region is not).
  /// The drive rides the same Raft the transport carries (`config_group_live` proves it over sim UDP).
  #[test]
  fn a_regional_council_commits_a_membership_change_at_a_majority() {
    let mut leader = council(OWNER);
    let mut follower = council(A);

    elect(&mut leader, &mut follower);
    assert!(
      leader.is_leader(),
      "the leader won the pre-vote then the real vote"
    );

    // Propose admitting a new member (not itself a voter); it commits and applies only at the majority.
    assert!(
      leader.propose(Reconfiguration::Admit(B)),
      "the leader appended the membership change"
    );
    replicate(&mut leader, &mut follower, 2);
    assert!(
      leader.configuration().members.contains(&B),
      "the membership change committed and applied at the leader"
    );
    assert!(
      follower.configuration().members.contains(&B),
      "and at the follower — the council agrees on the regional configuration"
    );
    assert!(
      leader.configuration().configuration_for(B).is_some(),
      "the admitted member now has a placement view derived from the regional configuration"
    );
  }

  /// AC (§4.8, D-14, the configuration master decides membership): the **leader** reconciles the regional
  /// membership from a SWIM alive set — a newly-alive host is proposed for admission, committed at the
  /// majority and applied on every voter; a **non-leader** proposes nothing; and a change **in flight** is
  /// not re-proposed (the caught-up gate keeps a not-yet-committed change from bloating the log each period).
  /// The retire-over-the-transport half is proven end to end in the daemon (`server/tests/fleet.rs`).
  #[test]
  fn the_leader_reconciles_regional_membership_from_the_alive_view() {
    let mut leader = council(OWNER);
    let mut follower = council(A);

    elect(&mut leader, &mut follower);
    assert!(leader.is_leader());

    // The region starts [OWNER, A]; host B has now joined the alive view.
    let alive = vec![OWNER, A, B];
    assert!(
      !follower.reconcile_alive(&alive),
      "a non-leader proposes nothing — only the leader is the configuration master"
    );
    assert!(
      leader.reconcile_alive(&alive),
      "the leader proposes admitting the new member"
    );
    assert!(
      !leader.reconcile_alive(&alive),
      "a change still in flight is not re-proposed (the caught-up gate — no duplicate log entry)"
    );

    // The entry commits at the majority and applies on both voters.
    replicate(&mut leader, &mut follower, 2);
    assert!(
      leader.configuration().members.contains(&B),
      "B is admitted to the regional membership at the leader"
    );
    assert!(
      follower.configuration().members.contains(&B),
      "and at the follower — the council agrees on the reconciled membership"
    );
    assert!(
      !leader.reconcile_alive(&alive),
      "the settled membership reconciles to a no-op — the log grows only for real changes"
    );
  }

  /// AC (§4.8, A-9 — "Host failure increments the host epoch"): the leader takes over a **failed** member by
  /// committing a `TakeOver` — bumping the member's fencing epoch AND retiring it — so a resumed stale owner
  /// is fenced by the advanced epoch (`StaleEpoch`), not only by the configuration generation. A holder
  /// raises its fence to this committed epoch on install (`server::fleet`; the FencedRegister TLA+
  /// revalidation the design mandates for A-9 is owed before the modeled result formally applies).
  #[test]
  fn the_leader_takes_over_a_failed_member_bumping_its_epoch() {
    let mut leader = council(OWNER);
    let mut follower = council(A);
    elect(&mut leader, &mut follower);
    assert!(leader.is_leader());

    let a_epoch_before = leader
      .configuration()
      .epochs
      .get(&A)
      .copied()
      .expect("A is a member");

    // A is no longer alive: the leader proposes its takeover (an epoch bump plus a retirement).
    assert!(
      leader.reconcile_alive(&[OWNER]),
      "the leader proposes the failed member's takeover"
    );
    replicate(&mut leader, &mut follower, 2);

    assert!(
      !leader.configuration().members.contains(&A),
      "the failed member is retired"
    );
    let a_epoch_after = leader
      .configuration()
      .epochs
      .get(&A)
      .copied()
      .expect("the retired member's epoch is kept, to keep fencing its records");
    assert!(
      a_epoch_after.0 > a_epoch_before.0,
      "the takeover bumped the failed member's fencing epoch ({} -> {})",
      a_epoch_before.0,
      a_epoch_after.0
    );
    assert!(
      follower.configuration().epochs.get(&A).copied() == Some(a_epoch_after),
      "and the follower applied the same bump — the council agrees on the fencing epoch"
    );
  }
}
