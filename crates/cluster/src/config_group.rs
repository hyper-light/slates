//! The configuration group (§4.8 "Configuration, by consensus", D-14) — the authority that maintains
//! the versioned [`Configuration`] every register request carries: membership, the neighbourhood
//! candidates are drawn from, the fault tolerance, and the host epochs. It is touched **only** on
//! membership, takeover, neighbourhood and home changes — never on a per-write path (banned item 10) —
//! and read per request from a local copy.
//!
//! The configuration is the deterministic fold of a **committed Raft log** ([`crate::raft`], the hecate
//! dialect): each change — admit, retire, takeover — is a [`ConfigCommand`] proposed to the log by the
//! leader and applied to the configuration only once committed, so every voter reaches the same
//! configuration, and the version bumps once per applied change so a request under the stale version is
//! refused (`ConfigurationStale`). Degenerate on a laptop (`f = 0`): the sole voter self-elects and its
//! append commits at once, so a change applies synchronously — the identical code path a fleet runs
//! through replication, never a mode switch (R8). The voter set is the config group's own membership
//! (fixed here); it is distinct from the volume neighbourhood the admit/retire commands grow.
//!
//! Built here: the change-through-the-log consensus (above), the SWIM-view [`reconcile`](ConfigGroup::reconcile)
//! bridge, and **takeover** ([`take_over`](ConfigGroup::take_over)) — when SWIM declares the owner dead,
//! the group reassigns the volume to the rendezvous-first survivor, bumps the host epoch, and advances
//! the generation, so a resumed stale owner is fenced (by the advanced configuration generation in this
//! single-generation model — `ConfigurationStale`/`ForeignGeneration`). The [`RegionalCouncil`] below now
//! runs this Raft **live over the fleet transport**: the daemon's record-plane coordinator drives its
//! election and replication (`slates_server::fleet`), so this module stays sans-io and the drive lives
//! there. Owed: joint consensus to change the voter set.
//! The per-host epoch fence and the new owner's phase-one recovery are now **built** for the
//! transport-driven register — `install_authority`/`prepare`/`promote_over_holders`
//! ([`slates_db::register`]) and the live `promote_record`/`promote_under_configuration`
//! ([`crate`]), oracle-tested for Continuity and StaleNeverCommits (single-value registers; the ledger's
//! committed-prefix adoption over the transport is the generalization, proven in the `slates-db` ledger
//! simulation and owed here). What remains at this seam is composing them into the takeover flow from
//! the owner runtime: distributing the taken-over authority to the holders (the `install_authority`
//! calls) and having the new owner run the promotion before it serves.

use std::mem::size_of;

use slates_db::register::{
  Configuration, DomainId, HostEpoch, HostId, ObjectId, Quorum, RegionalConfiguration,
  candidates_for, rendezvous_first, select_neighbourhood,
};

use crate::membership::Membership;
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
  /// Take over a dead owner's volume, reassigning `object` to the rendezvous-first survivor.
  TakeOver {
    /// The dead owner being taken over.
    dead: HostId,
    /// The volume object whose ownership moves (its 128-bit id, whose high half named the dead owner).
    object: ObjectId,
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
      ConfigCommand::TakeOver { dead, object } => {
        out.push(COMMAND_TAKE_OVER);
        out.extend_from_slice(&dead.0.to_le_bytes());
        out.extend_from_slice(&object.0);
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
      COMMAND_TAKE_OVER => {
        let (dead, rest) = take_host(rest)?;
        // The object is exactly its 16 id bytes; a different length is a corrupt entry.
        let bytes: [u8; size_of::<ObjectId>()] = rest.try_into().ok()?;
        Some(ConfigCommand::TakeOver {
          dead,
          object: ObjectId(bytes),
        })
      }
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
  /// Retire a member from the neighbourhood (a death the membership view confirmed).
  Retire(HostId),
}

/// A refusal to take over a dead owner's volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TakeoverError {
  /// The host being taken over is not the volume's current owner — a non-owner death is a neighbourhood
  /// [`Retire`](Reconfiguration::Retire), not a takeover.
  NotOwner {
    /// The current owner (the one a takeover would have to name).
    owner: HostId,
  },
  /// No survivor remains in the neighbourhood to take the volume — every candidate is gone. The volume
  /// is unrecoverable from this configuration (the correlated-loss case the neighbourhood bound makes
  /// rare); the caller reports the loss rather than inventing an owner.
  NoSurvivor,
}

/// The configuration group on one node: the [`RaftNode`] over the configuration-group voters (the
/// hecate Raft dialect, §4.8 mechanism 2) and the [`Configuration`] built by applying its committed log
/// in order. A change — admit, retire, takeover — is a command proposed to the log and applied to the
/// configuration only once committed, so every voter reaches the same configuration. At `f = 0` the sole
/// voter self-elects and its append commits at once, so a change applies synchronously — the same code
/// path as a fleet, never a mode switch (R8). The voter set is the config group's own membership (fixed
/// here); it is distinct from the volume neighbourhood the admit/retire commands grow, and changes only
/// by joint-consensus membership change (owed).
pub struct ConfigGroup {
  raft: RaftNode,
  configuration: Configuration,
  applied: u64,
  /// The scatter width (§4.8 "Placement", D-14) — the size the neighbourhood is bounded to, so the
  /// copyset count stays linear in the fleet (not `Θ(S^f)`) at any size. It defaults to the **candidate
  /// floor** `2f+1` ([`Quorum::candidates`]) — [`scatter_width`](slates_db::register::scatter_width) with
  /// the recovery term unsized, the tightest and lowest-loss neighbourhood, exactly one copyset (owner +
  /// `2f`) — and is raised by [`set_scatter`] to the derived width once a deployment sizes its recovery
  /// (data per host, re-replication bandwidth, recovery budget). `0` is the explicit *unbounded* escape
  /// (the whole alive set) a test uses; it is never a resting default, because unbounded rendezvous over
  /// the region is the `Θ(S^f)` data-loss case the bounding exists to prevent.
  scatter: u64,
}

impl ConfigGroup {
  /// A single-owner configuration group at fault tolerance `quorum` — one Raft voter (this `owner`),
  /// the configuration carrying `quorum` for the **volume neighbourhood** (the candidate holders the
  /// register path draws from). The voter set stays solo here — it is distinct from the neighbourhood
  /// the admit/retire commands grow (per this module's contract) — so the lone voter self-elects and
  /// may propose at once. This is the fleet degenerate the owner runtime ([`crate::fleet::FleetNode`])
  /// composes; [`solo`](ConfigGroup::solo) is this at `f = 0` (the laptop). `quorum`'s `f` comes from
  /// the failure-domain tree at deployment (a measured input, §4.8), or from a test's chosen topology.
  pub fn new(owner: HostId, quorum: Quorum) -> ConfigGroup {
    let mut raft = RaftNode::new(owner, vec![owner]);
    let _ = raft.start_election();
    let mut configuration = Configuration::solo(owner);
    // The neighbourhood quorum is the only thing that differs from the laptop configuration; the voter
    // set, version, epoch and (empty) neighbourhood are the same, grown later by reconcile.
    configuration.quorum = quorum;
    // Bound the neighbourhood from birth to the candidate floor `2f+1` (`scatter_width` with the recovery
    // term unsized) — one copyset, the lowest-loss placement — never the unbounded alive set; `set_scatter`
    // raises it once a deployment sizes its recovery bandwidth and budget.
    let scatter = u64::try_from(quorum.candidates()).unwrap_or(u64::MAX);
    ConfigGroup {
      raft,
      configuration,
      applied: 0,
      scatter,
    }
  }

  /// Raises the scatter width the neighbourhood is bounded to (§4.8 "Placement") above its default
  /// candidate floor — the derived [`scatter_width`](slates_db::register::scatter_width) a fleet passes
  /// once it has sized its recovery (data per host `D`, re-replication bandwidth `B`, budget `T`). A width
  /// above `2f+1` needs the fixed-copyset construction in placement to keep the copyset count linear (owed
  /// with the copyset rewrite of `candidates_for`); `0` is the explicit unbounded escape. Read by
  /// [`reconcile`]; the next reconcile applies it (growing an under-wide neighbourhood, bounding an
  /// over-wide one).
  pub fn set_scatter(&mut self, scatter: u64) {
    self.scatter = scatter;
  }

  /// Installs the fleet's failure-domain map (§4.8, D-14) — each host's declared domain from the deployment
  /// manifest — so placement forms copysets across distinct domains. A host absent from the map is its own
  /// domain (unique-per-host); an empty map is the laptop and any undeclared deployment (the prior
  /// behaviour). Set once at boot; the neighbourhood the map is read against changes with reconcile, the
  /// domain assignment does not.
  pub fn set_domains(&mut self, domains: std::collections::BTreeMap<HostId, DomainId>) {
    self.configuration.domains = domains;
  }

  /// The solo configuration group — one voter, `owner`, `f = 0`, version zero (the laptop degenerate).
  /// The lone voter elects itself leader at once, so it may immediately propose configuration changes.
  pub fn solo(owner: HostId) -> ConfigGroup {
    ConfigGroup::new(owner, Quorum { f: 0 })
  }

  /// The current configuration — read per request; a request carrying an older version is refused with
  /// this one ([`Configuration::check_version`]).
  pub fn configuration(&self) -> &Configuration {
    &self.configuration
  }

  /// Whether this node is the configuration group's leader (only the leader may propose changes).
  pub fn is_leader(&self) -> bool {
    self.raft.is_leader()
  }

  /// The number of committed configuration-log entries applied so far (each a proposed change; a no-op
  /// change is not proposed, so the log's growth tracks real configuration changes — the near-zero
  /// commit rate the design makes a tripwire).
  pub fn log_len(&self) -> usize {
    self.raft.committed_entries().len()
  }

  /// Proposes a reconfiguration through the configuration log and applies whatever newly commits (§4.8;
  /// the Raft consensus). A no-op change (a member already present, or absent, in the neighbourhood) is
  /// not proposed. Returns whether the configuration changed. At `f = 0` the proposal commits at once, so
  /// the change applies before returning; a non-leader cannot propose and returns `false`.
  pub fn reconfigure(&mut self, change: Reconfiguration) -> bool {
    let (command, would_change) = match change {
      Reconfiguration::Admit(host) => (
        ConfigCommand::Admit(host),
        !self.configuration.neighbourhood.contains(&host),
      ),
      Reconfiguration::Retire(host) => (
        ConfigCommand::Retire(host),
        host != self.configuration.owner && self.configuration.neighbourhood.contains(&host),
      ),
    };
    if !would_change {
      return false;
    }
    self.propose(command)
  }

  /// Proposes `command` to the configuration log and applies whatever newly commits. Returns whether the
  /// configuration changed. A non-leader cannot append and returns `false`.
  fn propose(&mut self, command: ConfigCommand) -> bool {
    if !self.raft.append_command(command.encode()) {
      return false;
    }
    self.apply_committed()
  }

  /// Applies every committed but not-yet-applied log entry to the configuration, in commit order, so the
  /// configuration is the deterministic fold of the committed log. Returns whether anything changed.
  fn apply_committed(&mut self) -> bool {
    let mut changed = false;
    let committed = self.raft.committed_entries().to_vec();
    while let Some(entry) = committed.get(usize::try_from(self.applied).unwrap_or(usize::MAX)) {
      if let Some(command) = ConfigCommand::decode(&entry.command) {
        changed |= self.apply_change(command);
      }
      self.applied = self.applied.saturating_add(1);
    }
    changed
  }

  /// Applies one committed command to the configuration — the deterministic mutation every voter makes.
  /// Returns whether the configuration changed (bumping the version once when it does, so a request under
  /// the stale version is refused). A command that no longer applies to the current state (a takeover of
  /// a host a prior committed entry already replaced) is a safe no-op.
  fn apply_change(&mut self, command: ConfigCommand) -> bool {
    let changed = match command {
      ConfigCommand::Admit(host) => {
        if self.configuration.neighbourhood.contains(&host) {
          false
        } else {
          self.configuration.neighbourhood.push(host);
          self
            .configuration
            .neighbourhood
            .sort_unstable_by_key(|h| h.0);
          true
        }
      }
      ConfigCommand::Retire(host) => {
        if host == self.configuration.owner || !self.configuration.neighbourhood.contains(&host) {
          false
        } else {
          self.configuration.neighbourhood.retain(|h| *h != host);
          true
        }
      }
      ConfigCommand::TakeOver { dead, object } => self.apply_take_over(dead, object),
    };
    if changed {
      self.configuration.version = self.configuration.version.saturating_add(1);
    }
    changed
  }

  /// Applies a committed takeover: reassigns the volume to the rendezvous-first survivor of the object's
  /// copyset, bumps the host epoch, and drops the dead owner. A no-op if `dead` is no longer the owner or
  /// no holder of the object survives (the caller validated both before proposing; this stays safe if the
  /// log order changed them).
  fn apply_take_over(&mut self, dead: HostId, object: ObjectId) -> bool {
    if dead != self.configuration.owner {
      return false;
    }
    // The successor is the rendezvous-first of the object's surviving holders — the copyset `candidates_for`
    // places it on (under `dead`, at this quorum), minus the dead host — so the new owner held a copy
    // (copyset-consistent with placement and the routing view; above the candidate floor a neighbourhood
    // host outside the object's copyset never held it and must never be named its owner).
    let object_survivors: Vec<HostId> = candidates_for(
      dead,
      &self.configuration.neighbourhood,
      &self.configuration.domains,
      object,
      self.configuration.quorum,
    )
    .into_iter()
    .filter(|host| *host != dead)
    .collect();
    let Some(successor) = rendezvous_first(&object_survivors, object) else {
      return false;
    };
    self.configuration.owner = successor;
    // Bump the authority so the dead owner's in-flight records are fenced (the next epoch, a monotonic
    // step like the version, never a tunable).
    self.configuration.host_epoch = HostEpoch(self.configuration.host_epoch.0.saturating_add(1));
    // The dead host leaves the neighbourhood entirely; the new owner keeps the rest of the survivors (not
    // just this object's copyset — its other objects have their own copysets within the same neighbourhood).
    let neighbourhood: Vec<HostId> = self
      .configuration
      .neighbourhood
      .iter()
      .copied()
      .filter(|host| *host != dead)
      .collect();
    self.configuration.neighbourhood = neighbourhood;
    true
  }

  /// Reconciles the configuration's neighbourhood with a SWIM membership `view`: admits every alive
  /// member not yet in the neighbourhood and retires every neighbourhood member no longer alive — the
  /// bridge from failure detection to the configuration authority. Returns whether the configuration
  /// changed (the version having advanced once per change). At `f > 0` each admit/retire is a consensus
  /// proposal (owed).
  pub fn reconcile(&mut self, view: &Membership) -> bool {
    let alive = view.alive();
    // The neighbourhood is the **bounded** scatter set the owner draws candidates from — not every alive
    // host (D-14: its size, the scatter width, bounds the copyset count, so it must not grow with the
    // fleet). `select_neighbourhood` picks the owner plus the top `scatter-1` alive hosts by rendezvous,
    // deterministically and stably; the width defaults to the candidate floor `2f+1` (one copyset) and is
    // raised only when a deployment sizes its recovery. Admit the selected, retire whatever fell out.
    let target = select_neighbourhood(self.configuration.owner, &alive, self.scatter);
    let mut changed = false;
    for &host in &target {
      changed |= self.reconfigure(Reconfiguration::Admit(host));
    }
    let stale: Vec<HostId> = self
      .configuration
      .neighbourhood
      .iter()
      .copied()
      .filter(|host| !target.contains(host))
      .collect();
    for host in stale {
      changed |= self.reconfigure(Reconfiguration::Retire(host));
    }
    changed
  }

  /// Takes over the volume from a dead owner (§4.8 "Promotion and takeover"): SWIM has declared the
  /// current owner `dead`, so the group assigns the volume to the survivor of the object's copyset that
  /// rendezvous ranks first for `object`, **bumps the host epoch** (the successor serves under the new
  /// epoch), drops the dead owner from the neighbourhood, and advances the version. Returns the new
  /// configuration, or a [`TakeoverError`] if the named host is not the owner or no holder survives.
  ///
  /// In this single-generation model a resumed stale owner is fenced by the advanced generation: it
  /// still holds the old configuration, so its request is refused `ConfigurationStale`, and a record it
  /// ships to a holder now serving the new generation is refused `ForeignGeneration`/`Unauthorized`. The
  /// design's per-host epoch fence — every holder raising its fence *for the dead host* to the new epoch,
  /// so the zombie is refused `StaleEpoch{new}` even under its own owner id — is the `FencedRegister`
  /// per-host model (A-9), owed. The new owner also still owes the phase-one recovery (reading the dead
  /// owner's highest records from the holders and adopting the head) before it serves. At `f > 0` the
  /// takeover decision is agreed by the configuration consensus (owed); this is its local effect.
  pub fn take_over(
    &mut self,
    dead: HostId,
    object: ObjectId,
  ) -> Result<&Configuration, TakeoverError> {
    // Validate against the current configuration before proposing — a takeover of a non-owner or one
    // with no survivor is refused without a log entry.
    if dead != self.configuration.owner {
      return Err(TakeoverError::NotOwner {
        owner: self.configuration.owner,
      });
    }
    // The object's surviving holders: the copyset `candidates_for` places it on, minus the dead host — the
    // same set `apply_take_over` chooses the successor from, so validation and application agree.
    let object_survivors: Vec<HostId> = candidates_for(
      dead,
      &self.configuration.neighbourhood,
      &self.configuration.domains,
      object,
      self.configuration.quorum,
    )
    .into_iter()
    .filter(|host| *host != dead)
    .collect();
    if rendezvous_first(&object_survivors, object).is_none() {
      return Err(TakeoverError::NoSurvivor);
    }
    // Propose the takeover through the configuration log; at f = 0 it commits and applies at once.
    self.propose(ConfigCommand::TakeOver { dead, object });
    Ok(&self.configuration)
  }
}

/// The **regional configuration council** on one node (§4.8, D-14 — the "configuration master", a small
/// elected council per region): the multi-voter [`RaftNode`] over the council voters, producing the
/// [`RegionalConfiguration`] every node learns. Membership changes commit at a majority over the transport
/// — the leader [`propose`](RegionalCouncil::propose)s, the followers serve, and each committed command
/// applies to the regional configuration — the distributed form of the per-node [`ConfigGroup`]. The drive
/// primitives ride the same [`RaftMessage`] wire the fleet transport carries.
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
    };
    if !would_change {
      return false;
    }
    self.raft.append_command(command.encode())
  }

  /// Whether the council's log is fully committed — nothing proposed is still in flight
  /// (`last_log_index == commit_index`). [`reconcile_alive`](RegionalCouncil::reconcile_alive) reads it so
  /// a change proposed but not yet committed is **not re-proposed** each period: unlike the solo
  /// [`ConfigGroup`], the council's [`propose`](RegionalCouncil::propose) commits only later over the
  /// transport, so `configuration.members` does not reflect the change until then, and an ungated reconcile
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
      proposed |= self.propose(Reconfiguration::Retire(host));
    }
    proposed
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
      ConfigCommand::TakeOver { dead, .. } => {
        self.configuration.take_over(dead, self.scatter);
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::membership::{Liveness, MemberState};

  const OWNER: HostId = HostId(1);
  const A: HostId = HostId(2);
  const B: HostId = HostId(3);
  const C: HostId = HostId(4);

  /// Admitting a member grows the neighbourhood and advances the version; a duplicate admit is a no-op.
  #[test]
  fn reconfigure_admits_a_member_advancing_the_version() {
    let mut group = ConfigGroup::solo(OWNER);
    assert_eq!(group.configuration().version, 0);
    assert_eq!(group.configuration().neighbourhood, vec![OWNER]);

    assert!(group.reconfigure(Reconfiguration::Admit(A)));
    assert_eq!(group.configuration().neighbourhood, vec![OWNER, A]);
    assert_eq!(
      group.configuration().version,
      1,
      "the version advanced on the change"
    );

    assert!(
      !group.reconfigure(Reconfiguration::Admit(A)),
      "a duplicate admit is a no-op"
    );
    assert_eq!(group.configuration().version, 1);
  }

  /// Retiring a member shrinks the neighbourhood and advances the version; the owner is never retired.
  #[test]
  fn reconfigure_retires_a_member_and_never_the_owner() {
    let mut group = ConfigGroup::solo(OWNER);
    group.reconfigure(Reconfiguration::Admit(A));

    assert!(group.reconfigure(Reconfiguration::Retire(A)));
    assert_eq!(group.configuration().neighbourhood, vec![OWNER]);
    assert_eq!(group.configuration().version, 2);

    assert!(
      !group.reconfigure(Reconfiguration::Retire(OWNER)),
      "the owner is never retired"
    );
    assert_eq!(group.configuration().version, 2);
  }

  /// Reconciling with a membership view admits alive peers and retires the dead — the SWIM-to-config
  /// bridge — advancing the version, and is a no-op when the view already matches.
  #[test]
  fn reconcile_tracks_the_membership_view() {
    // f=1: the candidate floor is 2f+1 = 3, so the owner and both alive peers fit the bounded
    // neighbourhood (at f=0 the owner is the only holder and no peer would be admitted — the bound is the
    // point). The neighbourhood's internal order is rendezvous-derived, not observable, so the assertions
    // test membership, not order (R5).
    let mut group = ConfigGroup::new(OWNER, Quorum { f: 1 });
    let mut view = Membership::new(OWNER);
    view.apply(
      A,
      MemberState {
        liveness: Liveness::Alive,
        incarnation: 0,
      },
    );
    view.apply(
      B,
      MemberState {
        liveness: Liveness::Alive,
        incarnation: 0,
      },
    );

    assert!(group.reconcile(&view), "alive peers are admitted");
    let mut admitted = group.configuration().neighbourhood.clone();
    admitted.sort_unstable_by_key(|host| host.0);
    assert_eq!(
      admitted,
      vec![OWNER, A, B],
      "the owner and both alive peers fill the f=1 candidate floor"
    );
    let after_admit = group.configuration().version;
    assert!(after_admit >= 2, "the version advanced once per admit");

    assert!(
      !group.reconcile(&view),
      "reconciling an unchanged view changes nothing"
    );
    assert_eq!(group.configuration().version, after_admit);

    // B dies; reconciling retires it from the neighbourhood.
    view.apply(
      B,
      MemberState {
        liveness: Liveness::Dead,
        incarnation: 0,
      },
    );
    assert!(group.reconcile(&view), "a dead peer is retired");
    let mut survivors = group.configuration().neighbourhood.clone();
    survivors.sort_unstable_by_key(|host| host.0);
    assert_eq!(
      survivors,
      vec![OWNER, A],
      "B is retired from the neighbourhood; the owner and the surviving peer remain"
    );
    assert!(group.configuration().version > after_admit);
  }

  /// Taking over a dead owner reassigns the volume to the rendezvous-first survivor of the
  /// neighbourhood, bumps the host epoch, drops the dead owner, and advances the generation.
  #[test]
  fn take_over_reassigns_to_the_rendezvous_first_survivor_and_bumps_the_epoch() {
    // f=1 with a floor neighbourhood [OWNER, A, B] — one copyset, so the object's surviving holders are
    // exactly [A, B] and the successor is the one they rendezvous-rank first (the copyset-consistent
    // choice; the above-floor multi-copyset case is proven separately).
    let mut group = ConfigGroup::new(OWNER, Quorum { f: 1 });
    group.reconfigure(Reconfiguration::Admit(A));
    group.reconfigure(Reconfiguration::Admit(B));
    assert_eq!(group.configuration().host_epoch, HostEpoch(1));
    let before = group.configuration().version;

    let object = ObjectId::new(OWNER, 42);
    let new = group
      .take_over(OWNER, object)
      .expect("a survivor takes over")
      .clone();

    let survivors = [A, B];
    assert!(survivors.contains(&new.owner), "a survivor took over");
    assert_eq!(
      new.owner,
      rendezvous_first(&survivors, object).unwrap(),
      "the rendezvous-first survivor is chosen"
    );
    assert_eq!(new.host_epoch, HostEpoch(2), "the host epoch is bumped");
    assert!(
      !new.neighbourhood.contains(&OWNER),
      "the dead owner left the neighbourhood"
    );
    assert!(new.version > before, "the generation advanced");
  }

  /// AC (§4.8 "Promotion and takeover", D-14): above the candidate floor the takeover assigns the object
  /// to a survivor of *that object's* copyset (a host that held a copy), and the new owner keeps the whole
  /// surviving neighbourhood — not just the one copyset — because its other objects have their own copysets
  /// within it.
  #[test]
  fn take_over_above_the_floor_picks_from_the_objects_copyset_and_keeps_the_neighbourhood() {
    // OWNER + four co-holders at f=1 → 2f=2 per copyset → two fixed copysets, above the floor of three.
    let mut group = ConfigGroup::new(OWNER, Quorum { f: 1 });
    for host in [A, B, C, HostId(5)] {
      group.reconfigure(Reconfiguration::Admit(host));
    }
    let before = group.configuration().neighbourhood.clone();
    assert_eq!(before.len(), 5, "a wide neighbourhood, above the floor");

    let object = ObjectId::new(OWNER, 7);
    // The object's holders under the dead owner — the copyset placement puts it on (the group's own
    // domain map, so this matches what the takeover computes internally).
    let holders = candidates_for(
      OWNER,
      &before,
      &group.configuration().domains,
      object,
      Quorum { f: 1 },
    );
    assert_eq!(
      holders.len(),
      3,
      "the object takes one copyset of 2f+1, not the whole neighbourhood"
    );

    let new = group
      .take_over(OWNER, object)
      .expect("a holder survives")
      .clone();
    assert!(
      holders.contains(&new.owner),
      "the successor held the object (is in its copyset)"
    );
    assert_ne!(new.owner, OWNER, "never the dead owner");
    assert!(
      !new.neighbourhood.contains(&OWNER),
      "the dead owner left the neighbourhood"
    );
    assert_eq!(
      new.neighbourhood.len(),
      before.len() - 1,
      "the new owner keeps the whole surviving neighbourhood, not just the object's copyset"
    );
  }

  /// Taking over a host that is not the current owner is refused — a non-owner death is a neighbourhood
  /// retire, not a takeover.
  #[test]
  fn take_over_of_a_non_owner_refuses() {
    let mut group = ConfigGroup::solo(OWNER);
    group.reconfigure(Reconfiguration::Admit(A));
    assert_eq!(
      group.take_over(A, ObjectId::new(OWNER, 42)),
      Err(TakeoverError::NotOwner { owner: OWNER }),
      "only the current owner is taken over"
    );
  }

  /// Taking over when no survivor remains is refused — the volume is unrecoverable from this
  /// configuration rather than assigned to a phantom owner.
  #[test]
  fn take_over_with_no_survivor_refuses() {
    let mut group = ConfigGroup::solo(OWNER);
    assert_eq!(
      group.take_over(OWNER, ObjectId::new(OWNER, 42)),
      Err(TakeoverError::NoSurvivor),
      "a lone owner has no survivor to take over"
    );
  }

  /// The solo group self-elects, so it may propose; each real change is committed through the log (the
  /// committed log grows only for changes that alter the configuration, not for no-ops).
  #[test]
  fn changes_are_committed_through_the_log() {
    let mut group = ConfigGroup::solo(OWNER);
    assert!(
      group.is_leader(),
      "the lone voter self-elects and may propose"
    );
    assert_eq!(group.log_len(), 0, "nothing committed yet");

    assert!(group.reconfigure(Reconfiguration::Admit(A)));
    assert_eq!(group.log_len(), 1, "the admit committed one log entry");
    assert!(
      !group.reconfigure(Reconfiguration::Admit(A)),
      "a duplicate admit is a no-op"
    );
    assert_eq!(
      group.log_len(),
      1,
      "a no-op change is not proposed, so the log does not grow"
    );

    assert!(group.reconfigure(Reconfiguration::Retire(A)));
    assert_eq!(group.log_len(), 2, "the retire committed a second entry");
    assert_eq!(group.configuration().neighbourhood, vec![OWNER]);
  }

  /// Each config command round-trips through encode/decode, and a malformed entry decodes to `None`
  /// (applied as a safe no-op rather than panicking).
  #[test]
  fn config_command_round_trips() {
    let commands = [
      ConfigCommand::Admit(A),
      ConfigCommand::Retire(B),
      ConfigCommand::TakeOver {
        dead: OWNER,
        object: ObjectId::new(OWNER, 0x1234),
      },
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
}
