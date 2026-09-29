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
//!
//! The **voter set follows the committed membership**: whenever a committed admit, retire or takeover frees
//! or fills a seat, the leader moves the Raft voter set through the core's joint-consensus change
//! ([`reconcile_voters`](RegionalCouncil::reconcile_voters)) to [`council_voters`] — up to the candidate
//! floor `2f + 1` seats, "a small elected council per region": every sitting voter still a member keeps its
//! seat, and a free seat goes only to a member the leader holds alive (the lowest id among them, a tiebreak
//! with no other meaning). A voter taken over leaves the consensus set and stops counting toward every
//! majority, and a live learner is promoted to its seat, so the council keeps tolerating `f` failures; an
//! admission while every seat is held moves no voter. Before 2026-09-13 the voter set was fixed at boot and
//! never shrank (`docs/bugs/2026-09-13-raft-voter-set-never-shrinks.md`); until 2026-09-22 it was the
//! lowest member ids whatever their state, so a replacement admitted beside its unretired predecessor took
//! a live voter's seat (`docs/bugs/2026-09-22-council-seats-follow-id-order-not-liveness.md`).
//! A member the leader promotes is **caught up before it votes** (thesis §4.2.1): `reconcile_voters` stages
//! the members its target adds — the leader replicates to them in rounds — and begins the joint change only
//! once each has completed a round within an election timeout, so the joint configuration never waits on a
//! voter with a stale log. Two kinds of non-voter therefore exist: every member beyond the seats fetches the
//! committed configuration (`adopt`); a member being promoted is also replicated to, as a staged learner.
//!
//! Owed: the FencedRegister TLA+ revalidation for the per-host epoch fence (A-9, §4.8 lines 1710-1712).

use std::mem::size_of;

use slates_db::register::{DomainId, HostId, Quorum, RegionalConfiguration};

use crate::fold::{Fold, Snapshotted};
use crate::raft::{
  CatchUp, ElectionPriority, RaftNode, RaftRecoveryError, SavedRaft, TimeoutNow, TransferRefusal,
};
use crate::raft_wire::{RaftMessage, decode_regional_configuration, encode_regional_configuration};

/// A regional configuration rides a council snapshot as the bytes a learner's fetch ships.
impl Snapshotted for RegionalConfiguration {
  fn encode(&self) -> Vec<u8> {
    encode_regional_configuration(self)
  }

  fn decode(bytes: &[u8]) -> Option<Self> {
    decode_regional_configuration(bytes).ok()
  }
}

/// A configuration change as it rides the Raft log — the command a committed [`LogEntry`](crate::raft::LogEntry)
/// carries, decoded and applied to the [`Configuration`] in commit order so every voter reaches the same
/// configuration. The Raft core treats it as opaque bytes; this is the config group's interpretation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigCommand {
  /// Admit a member to the neighbourhood, with the failure domain the operator declared for its node when
  /// there is one (task #22: the domains a region formed with are keyed by the member ids of that formation,
  /// so a restarted node's new id inherits its node's domain only through the admission that carries it).
  Admit {
    /// The member admitted.
    host: HostId,
    /// Its declared failure domain; `None` is unique-per-host, the default when none is declared.
    domain: Option<DomainId>,
  },
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
/// Format: an admission's domain is a presence byte — absent (unique-per-host) or present, in which case
/// the domain id follows as a little-endian u64.
const DOMAIN_ABSENT: u8 = 0;
const DOMAIN_PRESENT: u8 = 1;

impl ConfigCommand {
  /// The command's canonical bytes for the log: the tag, then the host id, then for an admission its
  /// domain (a presence byte, and the id when present), little-endian.
  pub fn encode(&self) -> Vec<u8> {
    let mut out = Vec::new();
    match self {
      ConfigCommand::Admit { host, domain } => {
        out.push(COMMAND_ADMIT);
        out.extend_from_slice(&host.0.to_le_bytes());
        match domain {
          Some(domain) => {
            out.push(DOMAIN_PRESENT);
            out.extend_from_slice(&domain.to_le_bytes());
          }
          None => out.push(DOMAIN_ABSENT),
        }
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
      COMMAND_ADMIT => {
        let (host, rest) = take_host(rest)?;
        let (&presence, rest) = rest.split_first()?;
        let domain = match presence {
          DOMAIN_ABSENT => None,
          DOMAIN_PRESENT => Some(take_word(rest)?.0),
          _ => return None,
        };
        Some(ConfigCommand::Admit { host, domain })
      }
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
  let (word, rest) = take_word(bytes)?;
  Some((HostId(word), rest))
}

/// Reads a little-endian u64 at the front of `bytes` — a host id, or an admission's domain id — returning
/// it and the remainder, or `None` if fewer than eight bytes remain.
fn take_word(bytes: &[u8]) -> Option<(u64, &[u8])> {
  if bytes.len() < size_of::<u64>() {
    return None;
  }
  let (head, rest) = bytes.split_at(size_of::<u64>());
  let mut word = [0u8; size_of::<u64>()];
  word.copy_from_slice(head);
  Some((u64::from_le_bytes(word), rest))
}

/// A proposed change to the configuration — the vocabulary the SWIM view and takeover speak to the
/// group. Applied locally at `f = 0`; carried through consensus at `f > 0` (owed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reconfiguration {
  /// Admit a member to the neighbourhood (a join the membership view learned), with the failure domain the
  /// operator declared for its node when there is one (`None` is unique-per-host).
  Admit {
    /// The member to admit.
    host: HostId,
    /// Its declared failure domain, carried into the configuration by the admission (task #22).
    domain: Option<DomainId>,
  },
  /// Retire a member from the neighbourhood (a clean departure — no fencing epoch bump).
  Retire(HostId),
  /// Take over a **failed** member (§4.8 "Promotion and takeover", line 1730 "Host failure increments the
  /// host epoch"): bump its fencing epoch **and** retire it, so a resumed stale owner's records under the
  /// old epoch are refused `StaleEpoch`. This is what the leader proposes for a SWIM-confirmed death, the
  /// per-host counterpart of a clean [`Retire`](Reconfiguration::Retire).
  TakeOver(HostId),
}

/// The voter set the council moves to (§4.8, D-14 — "a small elected council per region"), in id order: up
/// to the candidate floor `2f + 1` seats, so the council tolerates `f` voter failures while staying small in
/// a large region; the members beyond it are **learners** that fetch the committed configuration rather than
/// voting. At `2f + 1` members or fewer every member the leader holds alive votes.
///
/// - **A sitting voter keeps its seat while it is a member.** A seat is freed only by the member leaving the
///   configuration — a confirmed, stable death taken over, or a retirement — never by a suspicion: moving the
///   voter set on a revocable belief is the irreversible action the council's death-confirmation window
///   exists to prevent (`docs/bugs/2026-09-17-council-retires-a-suspected-voter.md`).
/// - **A free seat goes only to a member in `alive`** (the leader's authenticated-alive view), never to a
///   suspected or dead one, so every promotion adds a voter that can vote.
/// - **The id is only the tiebreak** among equally eligible members. A member id is derived from a
///   certificate and a per-boot nonce, so it carries no meaning for voting: a replacement's fresh id lands
///   anywhere in the order.
///
/// Until 2026-09-22 the voter set was the members with the lowest ids, whatever their state: a replacement
/// admitted beside its not-yet-retired predecessor took a live voter's seat — the leader's own, in the KIND
/// lane — while the dead predecessor kept one, and the replacement, which cannot observe its own
/// predecessor's death, then led a council that never retired it
/// (`docs/bugs/2026-09-22-council-seats-follow-id-order-not-liveness.md`). The voter set each node holds is
/// the one committed through the Raft log's joint changes, so only the leader computes this target; it need
/// not be a pure function of the members. More sitting members than seats (a lowered floor) keeps the ones
/// the leader holds alive first, then the lowest ids. Cost: `O(n log n)` in the members, once per leader
/// period.
pub fn council_voters(
  sitting: &[HostId],
  members: &[HostId],
  alive: &[HostId],
  quorum: Quorum,
) -> Vec<HostId> {
  let seats = quorum.candidates();
  let members: std::collections::BTreeSet<HostId> = members.iter().copied().collect();
  let alive: std::collections::BTreeSet<HostId> = alive.iter().copied().collect();
  let mut kept: Vec<HostId> = sitting
    .iter()
    .copied()
    .filter(|host| members.contains(host))
    .collect::<std::collections::BTreeSet<HostId>>()
    .into_iter()
    .collect();
  kept.sort_by_key(|host| !alive.contains(host));
  kept.truncate(seats);
  let free = seats.saturating_sub(kept.len());
  let promoted: Vec<HostId> = members
    .iter()
    .copied()
    .filter(|host| alive.contains(host) && !kept.contains(host))
    .take(free)
    .collect();
  kept.extend(promoted);
  kept.sort_unstable_by_key(|host| host.0);
  kept
}

/// The **regional configuration council** on one node (§4.8, D-14 — the "configuration master", a small
/// elected council per region): the multi-voter [`RaftNode`] over the council voters, producing the
/// [`RegionalConfiguration`] every node learns. Membership changes commit at a majority over the transport
/// — the leader [`propose`](RegionalCouncil::propose)s, the followers serve, and each committed command
/// applies to the regional configuration. The drive primitives ride the same [`RaftMessage`] wire the fleet
/// transport carries.
pub struct RegionalCouncil {
  raft: RaftNode,
  /// The regional configuration as the fold of the committed log over the state at the log's snapshot
  /// boundary (the formed configuration before the first compaction), compacted by the thesis's size rule
  /// ([`crate::fold`]). A fetched learner view stays in `learned`; replay advances the fold independently
  /// and applies each epoch change once.
  fold: Fold<RegionalConfiguration>,
  /// A newer read view fetched while a learner. It is never the input to Raft replay:
  /// applying its commands again would repeat epochs or home changes (§4.8, AUD-07).
  /// A changed learner view must survive before it is published to serving shards.
  view_pending: bool,
  learned: Option<RegionalConfiguration>,
  scatter: u64,
  /// Monotone count of the events that defer this node's own election — a leader's append answered here, or a
  /// vote this node granted a candidate (Raft Figure 2's two follower timer-resets). The drive loop's election
  /// timer reads it: while it advances, either a leader is alive or a candidate this node backed is still
  /// contesting the election, so this node does not campaign; once it stalls for the election timeout, contact
  /// is presumed lost and a pre-election begins.
  leader_contact: u64,
  /// A leader's invitation to campaign at once (thesis §3.10, leadership transfer), recorded by the serve
  /// path and acted on by the drive loop ([`invited_campaign`](Self::invited_campaign)), which alone starts
  /// elections and broadcasts — one slot, so bounded; a newer invitation replaces an older one.
  invitation: Option<TimeoutNow>,
  /// Stagings aborted by [`reconcile_voters`](Self::reconcile_voters) (the non-vacuity counter).
  stagings_aborted: u64,
}

impl RegionalCouncil {
  /// A fresh member with no authority to vote or campaign (§4.8, AUD-07). Its first state must
  /// come from an existing group, or from the explicit first-time bootstrap operation.
  pub fn learner(node: HostId, quorum: Quorum, scatter: u64, has_mirror: bool) -> RegionalCouncil {
    let mut group = Self::new(
      node,
      Vec::new(),
      Vec::new(),
      quorum,
      Default::default(),
      scatter,
      has_mirror,
    );
    let mut base = group.fold.base().clone();
    base.version = 0;
    group.fold = Fold::new(base);
    group
  }

  /// Whether this node has received the group's common base and consensus prefix.
  pub fn initialized(&self) -> bool {
    !self.raft.all_voters().is_empty()
  }

  /// The consensus prefix and its common fold base for a fresh member. An uninitialized node
  /// has no group state to offer. The transport authenticates the sender and bounds the frame.
  pub fn join_state(&self) -> Option<(SavedRaft, RegionalConfiguration)> {
    self
      .initialized()
      .then(|| (self.raft.saved(), self.fold.base().clone()))
  }

  /// Creates the first voter of an explicitly authorized replacement group (§4.8).
  /// The caller must fence the former group and bind approval to this recovered application
  /// state. Ordinary bootstrap, startup and discovery never call this operation.
  pub fn reform(node: HostId, configuration: RegionalConfiguration, scatter: u64) -> Self {
    let mut group = Self::new(
      node,
      configuration.members.clone(),
      vec![node],
      configuration.quorum,
      configuration.domains.clone(),
      scatter,
      configuration.has_mirror,
    );
    group.fold = Fold::new(configuration);
    group.apply_committed();
    group
  }

  /// Restores this voter's complete retained state without clearing its vote (§4.8).
  /// A sole voter runs the ordinary election rule; a fleet voter waits for its peers.
  pub fn restore_from(
    &mut self,
    saved: SavedRaft,
    base: RegionalConfiguration,
  ) -> Result<(), RaftRecoveryError> {
    if saved.id != self.raft.id() {
      return Err(RaftRecoveryError::ReusedIdentity);
    }
    let fold = Fold::for_saved(&saved, base)?;
    self.raft = RaftNode::restore(saved)?;
    self.fold = fold;
    self.learned = None;
    self.apply_committed();
    if self.raft.all_voters() == [self.raft.id()] {
      self.election_timeout();
    }
    Ok(())
  }

  /// The retained scatter bound used when replaying the application base.
  pub fn scatter(&self) -> u64 {
    self.scatter
  }

  /// Whether a consensus transition still needs its anchor publication.
  pub fn retention_pending(&self) -> bool {
    self.view_pending || self.raft.retention_pending()
  }

  /// Stops proposing while an authorized replacement is fetched, retaining the old prefix.
  pub fn suspend(&mut self) {
    self.raft.suspend();
  }

  /// Records successful publication by the caller that owns the anchor.
  pub fn retained(&mut self) {
    self.view_pending = false;
    self.raft.retained();
  }

  /// Installs the group's state once under this member's fresh identity. A later fetch cannot
  /// erase a vote or replace the log: initialized nodes advance through Raft messages.
  pub fn join_from(
    &mut self,
    mut saved: SavedRaft,
    base: RegionalConfiguration,
  ) -> Result<(), RaftRecoveryError> {
    if self.initialized() {
      return Err(RaftRecoveryError::AlreadyInitialized);
    }
    if saved.id == self.raft.id() {
      return Err(RaftRecoveryError::ReusedIdentity);
    }
    // A compacted donor's base is its snapshot's state; a snapshot paired with another base would fold a
    // different history, so it is refused before anything is installed.
    let fold = Fold::for_saved(&saved, base)?;
    saved.id = self.raft.id();
    saved.voted_for = None;
    let raft = RaftNode::restore(saved)?;
    self.raft = raft;
    self.fold = fold;
    self.learned = None;
    self.apply_committed();
    Ok(())
  }

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
    let mut group = RegionalCouncil {
      raft,
      fold: Fold::new(configuration),
      learned: None,
      view_pending: false,
      scatter,
      leader_contact: 0,
      invitation: None,
      stagings_aborted: 0,
    };
    group.finish_election(false);
    group
  }

  /// The regional configuration the council has agreed on so far — every node's placement view derives from
  /// it ([`RegionalConfiguration::configuration_for`]).
  pub fn configuration(&self) -> &RegionalConfiguration {
    self.learned.as_ref().unwrap_or(self.fold.state())
  }

  /// The compactions this council's log has taken (the thesis's size rule, [`crate::fold`]).
  pub fn compactions(&self) -> u64 {
    self.fold.compactions()
  }

  /// The index through which this council's log is folded into its snapshot (zero before the first
  /// compaction).
  pub fn snapshot_index(&self) -> u64 {
    self.raft.snapshot_index()
  }

  /// The index of this council's last log entry.
  pub fn last_log_index(&self) -> u64 {
    self.raft.last_log_index()
  }

  /// Whether this node leads the council (only the leader may propose).
  pub fn is_leader(&self) -> bool {
    self.raft.is_leader()
  }

  /// The council's voter set in force (Raft §6): its base voters, and the incoming voters too while a joint
  /// change is in flight — who elections ask and learners fetch from.
  pub fn voters(&self) -> Vec<HostId> {
    self.raft.all_voters()
  }

  /// The peers the drive loop replicates to ([`RaftNode::replication_targets`]): the voters, plus — while a
  /// membership change's entry is still uncommitted — the voters it is removing, so a live member demoted to
  /// learner receives the entry that removes it, plus the members being caught up to join (thesis §4.2.1).
  /// Until 2026-09-28 this list was `voters`, which the recovery plan and the drive's elections read as the
  /// voter set: once staged members joined it, a council of one voter reported three, and a drain found no
  /// one to hand leadership to.
  pub fn replication_targets(&self) -> Vec<HostId> {
    self.raft.replication_targets()
  }

  /// The voting set after the membership transition and its log have committed. During
  /// a joint change this is absent: observing application membership alone cannot prove admission.
  pub fn committed_voters(&self) -> Option<Vec<HostId>> {
    (self.initialized() && self.caught_up() && !self.raft.in_joint_configuration())
      .then(|| self.raft.all_voters())
  }

  /// The leader-contact count (see the field): the drive loop's election timer resets while this advances.
  pub fn leader_contact(&self) -> u64 {
    self.leader_contact
  }

  /// A diagnostic dump of the council's Raft and applied state, for a membership-commit failure (never on a
  /// normal path): the role and term, the voter set and whether a joint change is in flight, the commit and
  /// last-log indexes, the applied member set and its version, and the committed log's configuration entries
  /// — each config command (admit / retire / takeover) or voter-configuration entry, with its term — so a
  /// stalled reconfiguration names what was proposed and committed, and when, not merely the outcome.
  pub fn debug_state(&self) -> String {
    let log: Vec<String> = self
      .raft
      .committed_entries()
      .iter()
      .map(
        |entry| match (&entry.config, ConfigCommand::decode(&entry.command)) {
          (Some(config), _) => format!("t{} cfg{config:?}", entry.term),
          (None, Some(command)) => format!("t{} {command:?}", entry.term),
          (None, None) => format!("t{} noop", entry.term),
        },
      )
      .collect();
    format!(
      "role={:?} term={} voters={:?} joint={} commit={} last_log={} applied={} members={:?} v{} log=[{}]",
      self.raft.role(),
      self.raft.term(),
      self.raft.all_voters(),
      self.raft.in_joint_configuration(),
      self.raft.commit_index(),
      self.raft.last_log_index(),
      self.fold.applied(),
      self.fold.state().members,
      self.fold.state().version,
      log.join(" | ")
    )
  }

  /// **DRIVE**: begins a **pre-election** on an election timeout (Raft §9.6), returning the [`PreVote`]s to
  /// ship to the other voters — asked *without inflating the term*, so a partitioned node cannot disrupt a
  /// healthy leader. A lone voter proceeds straight to leading with no messages (the `f = 0` degenerate).
  pub fn election_timeout(&mut self) -> Vec<RaftMessage> {
    let was_leader = self.is_leader();
    let messages = self
      .raft
      .on_election_timeout()
      .into_iter()
      .map(RaftMessage::PreVote)
      .collect();
    self.finish_election(was_leader);
    messages
  }

  /// A new leader appends a current-term no-op (Raft §5.4.2), so an inherited uncommitted
  /// tail can commit before the caught-up reconfiguration gate is consulted. Empty commands
  /// change no application state. A singleton applies its local majority immediately.
  fn finish_election(&mut self, was_leader: bool) {
    if !was_leader && self.is_leader() {
      self.raft.append_command(Vec::new());
      self.apply_committed();
    }
  }

  /// What the leader replicates to `follower` now — an append carrying at most `budget` entry bytes (a
  /// heartbeat when nothing is owed), or the snapshot when the entries it needs were compacted away (Raft
  /// §7) — or `None` when this node does not lead.
  pub fn replication_for(&self, follower: HostId, budget: usize) -> Option<RaftMessage> {
    self
      .raft
      .replicate_to(follower, budget)
      .map(RaftMessage::AppendEntries)
      .or_else(|| {
        self
          .raft
          .install_snapshot_for(follower)
          .map(RaftMessage::InstallSnapshot)
      })
  }

  /// **DRIVE**: the leader's CheckQuorum tick (Raft §6.2), on the election-timeout cadence: a leader that has
  /// not heard from a majority of voters since the previous tick **steps down**, so a leader cut off from its
  /// followers stops acting as one — it neither blocks the majority side's fresh election nor sits on a term
  /// it can no longer hold — and the contact window resets. The window is fed by the append replies the
  /// drive loop folds, timely or late ([`fold_reply`](RegionalCouncil::fold_reply)). A non-leader is
  /// unaffected; the sole voter is its own majority and never steps down (the laptop degenerate, R8).
  pub fn check_quorum(&mut self) {
    self.raft.check_quorum();
  }
  /// **DRIVE**: the election a leader invited this node to start (thesis §3.10), if an invitation arrived
  /// since the last call: the vote requests to broadcast, exactly as an election timeout's pre-votes are
  /// broadcast — no pre-vote round, since the leader invited it. Empty when no invitation is pending, or when
  /// the invitation is from another term or this node leads or does not vote (the core ignores it).
  pub fn invited_campaign(&mut self) -> Vec<RaftMessage> {
    let Some(invitation) = self.invitation.take() else {
      return Vec::new();
    };
    let was_leader = self.is_leader();
    let votes = self
      .raft
      .on_timeout_now(invitation)
      .into_iter()
      .map(RaftMessage::RequestVote)
      .collect();
    self.finish_election(was_leader);
    votes
  }

  /// **DRIVE**: starts a leadership transfer to `target` (thesis §3.10); refused, typed, when this node does
  /// not lead, `target` is itself or not a voter, or a transfer is already in flight. While it is in flight
  /// the leader refuses proposals; [`take_timeout_now`](Self::take_timeout_now) yields the invitation once
  /// the target has caught up; it ends when this node stops leading or is aborted after an election timeout.
  pub fn transfer_leadership(&mut self, target: HostId) -> Result<(), TransferRefusal> {
    self.raft.transfer_leadership(target)
  }

  /// **DRIVE**: the invitation to ship once the transfer's target has caught up — `(target, message)`, sent
  /// once — or `None`.
  pub fn take_timeout_now(&mut self) -> Option<(HostId, RaftMessage)> {
    self
      .raft
      .take_timeout_now()
      .map(|(target, invitation)| (target, RaftMessage::TimeoutNow(invitation)))
  }

  /// The leader this node knows for its current term — itself while it leads, else the leader whose append
  /// it last accepted (a hint, never consulted for safety); `None` while none is known.
  pub fn leader(&self) -> Option<HostId> {
    self.raft.leader()
  }

  /// The target of the leadership transfer in flight, if one is.
  pub fn transferring_to(&self) -> Option<HostId> {
    self.raft.transferring_to()
  }

  /// The voter a leader handing off should pick: the most caught-up other voter (thesis §3.10), or `None`
  /// when this node does not lead or votes alone.
  pub fn most_caught_up_voter(&self) -> Option<HostId> {
    self.raft.most_caught_up_voter()
  }

  /// **SERVE**: answers a request received over the transport — a pre-vote, a vote request, or an append —
  /// returning the reply to ship back and applying whatever newly committed to the regional configuration
  /// (a follower applies on the append). A reply (`VoteReply`/`PreVoteReply`/`AppendReply`) is not a request
  /// and is not answered here — its sender folds it with [`fold_reply`](RegionalCouncil::fold_reply).
  pub fn answer(&mut self, request: RaftMessage) -> Option<RaftMessage> {
    if !self.initialized() {
      return None;
    }
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
      RaftMessage::TimeoutNow(invitation) => {
        // No reply: the invited node's vote requests are its answer, broadcast by the drive loop.
        self.invitation = Some(invitation);
        None
      }
      RaftMessage::InstallSnapshot(snapshot) => {
        // A leader's snapshot is contact from it exactly as its append is (the timer rule above).
        let snapshot_term = snapshot.term;
        let scatter = self.scatter;
        let reply = self
          .fold
          .install(&mut self.raft, snapshot, |configuration, command| {
            apply_command(configuration, command, scatter);
          });
        if snapshot_term >= reply.term {
          self.leader_contact = self.leader_contact.saturating_add(1);
        }
        self.discard_superseded_view();
        Some(RaftMessage::InstallSnapshotReply(reply))
      }
      RaftMessage::VoteReply(_)
      | RaftMessage::PreVoteReply(_)
      | RaftMessage::AppendReply(_)
      | RaftMessage::InstallSnapshotReply(_) => None,
    }
  }

  /// **DRIVE**: folds a reply to one of this node's requests, returning any follow-on messages to ship — a
  /// granted pre-vote majority yields the real [`RequestVote`]s (the pre-election succeeded, so the term is
  /// advanced only now); a vote reply or append reply yields none. Applies whatever newly committed to the
  /// regional configuration on an append reply (the leader once a majority acknowledges).
  pub fn fold_reply(&mut self, reply: RaftMessage) -> Vec<RaftMessage> {
    let was_leader = self.is_leader();
    let messages = match reply {
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
      RaftMessage::InstallSnapshotReply(reply) => {
        self.raft.on_install_snapshot_reply(reply);
        self.apply_committed();
        Vec::new()
      }
      RaftMessage::PreVote(_)
      | RaftMessage::RequestVote(_)
      | RaftMessage::AppendEntries(_)
      | RaftMessage::TimeoutNow(_)
      | RaftMessage::InstallSnapshot(_) => Vec::new(),
    };
    self.finish_election(was_leader);
    messages
  }

  /// Proposes a membership change on the leader **without waiting**: it commits — and applies to the
  /// regional configuration — only once a majority of the council acknowledge it over the transport. Returns
  /// whether the leader appended it (a non-leader, or a change already reflected in the membership, returns
  /// `false`).
  pub fn propose(&mut self, change: Reconfiguration) -> bool {
    let (command, would_change) = match change {
      Reconfiguration::Admit { host, domain } => (
        ConfigCommand::Admit { host, domain },
        !self.fold.state().members.contains(&host),
      ),
      Reconfiguration::Retire(host) => (
        ConfigCommand::Retire(host),
        self.fold.state().members.contains(&host),
      ),
      Reconfiguration::TakeOver(host) => (
        // Per-host takeover: bump the host's fencing epoch and retire it (one bump fences every object it
        // owned; the surviving owner of each is recomputed by rendezvous, not named here).
        ConfigCommand::TakeOver { dead: host },
        self.fold.state().members.contains(&host),
      ),
    };
    if !would_change {
      return false;
    }
    let appended = self.raft.append_command(command.encode());
    // A single-voter bootstrap already reached its majority. Apply any local commit here;
    // waiting for a remote append reply would leave its membership frozen forever (AUD-07).
    self.apply_committed();
    appended
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

  /// Reconciles the regional membership with this leader's SWIM view **as the leader** (§4.8 "the
  /// configuration master decides membership"): proposes admitting every `alive` host not yet a member, and
  /// taking over — an epoch bump plus a retirement — every `dead` host still a member, each through the
  /// council log so it commits at a majority over the transport and applies on every voter. Each alive host
  /// comes with the failure domain its node declares, so an admission carries it into the configuration
  /// (task #22: a restarted node's new id inherits its node's domain this way).
  ///
  /// A member SWIM has only **suspected** — a probe unanswered, its death not yet confirmed — is in neither
  /// set, so it is left in the configuration until its suspicion resolves to death (retire) or is refuted
  /// (kept). This is the suspicion window applied to the council: a single missed probe, routine under load
  /// and likeliest against a fresh joiner, must not retire a live voter — retiring it there discards a live
  /// member and can leave the survivors of a later loss unable to reach a majority
  /// (`docs/bugs/2026-09-17-council-retires-a-suspected-voter.md`). The caller passes only members whose
  /// death it has **confirmed**, never merely suspected.
  ///
  /// Returns whether anything was proposed. A non-leader proposes nothing — the leader is the one
  /// configuration master and decides from its own SWIM view, so a follower's own detection need not
  /// propose. Gated on [`caught_up`](RegionalCouncil::caught_up) so a change in flight is not re-proposed; a
  /// host already a member (admit) or no longer one (takeover) is not proposed either (`propose`'s own
  /// `would_change` gate), so the log grows only for real changes.
  pub fn reconcile_alive(&mut self, alive: &[(HostId, Option<DomainId>)], dead: &[HostId]) -> bool {
    if !self.is_leader() || !self.caught_up() {
      return false;
    }
    let mut proposed = false;
    for &(host, domain) in alive {
      proposed |= self.propose(Reconfiguration::Admit { host, domain });
    }
    for &host in dead {
      // A member SWIM confirmed **dead** failed, so take it over — bump its fencing epoch and retire it
      // (§4.8 line 1730 "Host failure increments the host epoch"), not a clean retire. A host that is not a
      // current member is a no-op (`propose`'s `would_change` gate), so a confirmed-dead non-member is safe.
      proposed |= self.propose(Reconfiguration::TakeOver(host));
    }
    proposed
  }

  /// Whether `node` is a **voter** of this council — a member of the Raft consensus set in effect now. A
  /// non-voter (a **learner**) does not vote; it learns the committed configuration by fetching it from a
  /// voter and [`adopt`](RegionalCouncil::adopt)ing it. The drive loop reads this to take the voter path
  /// (drive the Raft) or the learner path (fetch). A voter a committed change removed is no longer one.
  pub fn is_voter(&self, node: HostId) -> bool {
    self.raft.is_voter(node)
  }

  /// **DRIVE** (leader): keeps the council's Raft voter set at [`council_voters`] of the committed membership,
  /// the voters in force, and `alive` — this leader's authenticated-alive view — one joint change at a time
  /// (Raft §6): when a committed admit, retire or takeover has freed or filled a seat so that the target
  /// differs from the voter set in force, the leader begins the joint change to it; once that entry has
  /// committed (the log is caught up again) it completes the change; and once `C_new` has committed the
  /// voters match the target and this is a no-op. Gated on [`caught_up`](RegionalCouncil::caught_up) like
  /// [`reconcile_alive`](RegionalCouncil::reconcile_alive), so a change in flight is never re-proposed.
  /// Returns whether it appended a configuration entry. A voter taken over thereby leaves the consensus set —
  /// it stops counting toward every majority — and a member the leader holds alive is promoted to its seat,
  /// so the council keeps tolerating `f` failures; admitting a member while every seat is held moves no
  /// voter. A leader the target no longer names (a lowered floor) steps down once `C_new` commits (the core's
  /// rule) and the new voters elect among themselves.
  pub fn reconcile_voters(&mut self, alive: &[HostId]) -> bool {
    if !self.is_leader() || !self.caught_up() {
      return false;
    }
    if self.raft.in_joint_configuration() {
      // The joint entry has committed: leave the joint phase for the new voter set alone.
      return self.raft.complete_membership_change();
    }
    let sitting = self.raft.all_voters();
    let target = council_voters(
      &sitting,
      &self.fold.state().members,
      alive,
      self.fold.state().quorum,
    );
    if target.is_empty() || target == sitting {
      // Nothing to add: unstage anything a previous target named.
      self.raft.catch_up(&sitting);
      return false;
    }
    // A member the change adds is caught up first (thesis §4.2.1), so the joint configuration never waits on
    // a voter with a stale log — the availability gap of the thesis's Figure 4.4.
    match self.raft.catch_up(&target) {
      CatchUp::Ready => self.raft.begin_membership_change(target),
      CatchUp::Aborted { .. } => {
        self.stagings_aborted = self.stagings_aborted.saturating_add(1);
        false
      }
      CatchUp::Pending | CatchUp::NotLeader => false,
    }
  }

  /// Whether this leader is catching `node` up to join the voters (thesis §4.2.1). The drive keeps direct
  /// contact with a staged member so the rounds can reach it.
  pub fn is_staged(&self, node: HostId) -> bool {
    self.raft.staged().iter().any(|(member, _)| *member == node)
  }

  /// The stagings this council aborted — a member that could not catch up — over its life.
  pub fn stagings_aborted(&self) -> u64 {
    self.stagings_aborted
  }

  /// Records this node's own election priority, as its drive measured it this period (§3.4).
  pub fn set_priority(&mut self, priority: ElectionPriority) {
    self.raft.set_priority(priority);
  }

  /// This node's election rank among the voters the caller holds `alive` (§3.4,
  /// [`RaftNode::election_rank`]): the timer yields one timeout per rank.
  pub fn election_rank(&self, alive: &[HostId]) -> usize {
    self.raft.election_rank(alive)
  }

  /// Hands leadership to a live, caught-up voter that outranks this leader, when one does (§3.4 with thesis
  /// §3.10, [`RaftNode::priority_transfer`]); the drive ships the invitation as for any transfer.
  pub fn priority_transfer(&mut self, alive: &[HostId]) -> Option<HostId> {
    self.raft.priority_transfer(alive)
  }

  /// The priority transfers this node started as leader, over its life.
  pub fn priority_transfers(&self) -> u64 {
    self.raft.priority_transfers()
  }

  /// Adopts a configuration a **learner** fetched from a council voter (§4.8, D-14: the council is a small
  /// elected set, so a non-voter member learns the committed configuration rather than voting on it).
  /// Returns whether it advanced — a fetch that is not newer than what this node already has (it is current,
  /// or the fetch raced a newer local view) is ignored, so adoption only moves forward. Only the drive
  /// loop's learner branch calls this; a voter's configuration is the deterministic fold of its Raft log.
  pub fn adopt(&mut self, configuration: RegionalConfiguration) -> bool {
    if configuration.version <= self.configuration().version {
      return false;
    }
    self.view_pending = true;
    self.learned = Some(configuration);
    true
  }

  /// Applies each newly committed command once from the common base, and compacts the log when the
  /// thesis's size rule says so ([`crate::fold`]). A fetched read view never feeds this fold and is
  /// discarded when replay reaches its version (§4.8, AUD-07).
  fn apply_committed(&mut self) {
    let scatter = self.scatter;
    self.fold.advance(&mut self.raft, |configuration, command| {
      apply_command(configuration, command, scatter);
    });
    self.discard_superseded_view();
  }

  /// Drops a fetched read view once the fold has reached its version.
  fn discard_superseded_view(&mut self) {
    if self
      .learned
      .as_ref()
      .is_some_and(|learned| learned.version <= self.fold.state().version)
    {
      self.learned = None;
    }
  }
}

/// Applies one committed entry's command bytes to the regional configuration; a configuration entry's
/// (empty) or a malformed command's bytes change nothing.
fn apply_command(configuration: &mut RegionalConfiguration, command: &[u8], scatter: u64) {
  match ConfigCommand::decode(command) {
    Some(ConfigCommand::Admit { host, domain }) => {
      configuration.admit(host, domain, scatter);
    }
    Some(ConfigCommand::Retire(host)) => {
      configuration.retire(host, scatter);
    }
    Some(ConfigCommand::TakeOver { dead }) => {
      configuration.take_over(dead, scatter);
    }
    None => {}
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Shape: an append budget no batch reaches, for the tests of every rule but batching and compaction.
  const UNBOUNDED: usize = usize::MAX;

  /// AC-8.1, §4.8, AUD-07: a replacement imports the original fold base, becomes a voter
  /// through joint consensus, then refuses an initial-state replay that would erase its vote.
  #[test]
  fn a_fresh_regional_member_joins_the_prefix_without_reusing_a_vote() {
    let mut groups = councils(&[OWNER, A, B]);
    elect_among(&mut groups, OWNER, &[OWNER, A]);
    let leader = groups.get_mut(&OWNER).unwrap();
    assert!(leader.propose(Reconfiguration::TakeOver(B)));
    assert!(leader.propose(Reconfiguration::Admit {
      host: C,
      domain: None
    }));
    replicate_round(&mut groups, OWNER, &[OWNER, A]);
    replicate_round(&mut groups, OWNER, &[OWNER, A]);
    let (saved, base) = groups[&OWNER].join_state().unwrap();
    let expected = groups[&OWNER].configuration().clone();
    let mut joining = RegionalCouncil::learner(C, Quorum { f: 1 }, 3, false);
    assert!(joining.election_timeout().is_empty());
    joining.join_from(saved.clone(), base.clone()).unwrap();
    assert_eq!(
      joining.configuration(),
      &expected,
      "takeover is folded once from the original base"
    );
    assert!(!joining.is_voter(C));
    let expected = fetch_ahead_of_regional_log(&mut groups, &mut joining);
    groups.insert(C, joining);
    // Two configuration entries (joint, then final), each delivered and committed in two rounds.
    for _ in 0..(2 * 2) {
      groups
        .get_mut(&OWNER)
        .unwrap()
        .reconcile_voters(&[OWNER, A, C]);
      replicate_round(&mut groups, OWNER, &[OWNER, A, C]);
    }
    assert!(groups[&C].is_voter(C));
    assert!(!groups[&C].is_voter(B));
    assert_join_vote_retained(groups.get_mut(&C).unwrap(), saved, base, C);
    assert_eq!(groups[&C].configuration(), &expected);
  }

  /// Fetch a later takeover without its log suffix; promotion must not apply it twice.
  fn fetch_ahead_of_regional_log(
    groups: &mut std::collections::BTreeMap<HostId, RegionalCouncil>,
    joining: &mut RegionalCouncil,
  ) -> RegionalConfiguration {
    assert!(
      groups
        .get_mut(&OWNER)
        .unwrap()
        .propose(Reconfiguration::TakeOver(A))
    );
    replicate_round(groups, OWNER, &[OWNER, A]);
    replicate_round(groups, OWNER, &[OWNER, A]);
    let expected = groups[&OWNER].configuration().clone();
    assert!(joining.adopt(expected.clone()));
    expected
  }

  /// A late join response cannot erase an initialized member's first vote (AUD-07).
  fn assert_join_vote_retained(
    replacement: &mut RegionalCouncil,
    saved: SavedRaft,
    base: RegionalConfiguration,
    replacement_id: HostId,
  ) {
    let (prefix, _) = replacement.join_state().unwrap();
    // A candidate exactly as up to date as the replacement: its last index and term, past any compaction.
    let request = crate::raft::RequestVote {
      term: prefix.term + 1,
      candidate: OWNER,
      last_log_index: prefix.snapshot_index + prefix.log.len() as u64,
      last_log_term: prefix
        .log
        .last()
        .map_or(prefix.snapshot_term, |entry| entry.term),
    };
    assert!(
      matches!(replacement.answer(RaftMessage::RequestVote(request)),
      Some(RaftMessage::VoteReply(reply)) if reply.granted && reply.voter == replacement_id)
    );
    assert_eq!(
      replacement.join_from(saved, base),
      Err(RaftRecoveryError::AlreadyInitialized)
    );
    assert!(
      matches!(replacement.answer(RaftMessage::RequestVote(crate::raft::RequestVote { candidate: A, ..request })),
      Some(RaftMessage::VoteReply(reply)) if !reply.granted)
    );
  }

  const OWNER: HostId = HostId(1);
  const A: HostId = HostId(2);
  const B: HostId = HostId(3);

  /// Each config command round-trips through encode/decode, and a malformed entry decodes to `None`
  /// (applied as a safe no-op rather than panicking).
  #[test]
  fn config_command_round_trips() {
    let commands = [
      ConfigCommand::Admit {
        host: A,
        domain: Some(DECLARED_DOMAIN),
      },
      ConfigCommand::Admit {
        host: B,
        domain: None,
      },
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

  /// Shape: a failure domain an admission carries in these tests — any id, distinct from unique-per-host.
  const DECLARED_DOMAIN: DomainId = 7;
  /// A fourth host, admitted without a declared domain.
  const C: HostId = HostId(4);

  /// An admission carries the member's declared failure domain into the configuration (task #22 — a
  /// restarted node's new id inherits its node's domain through this), and one without a declaration stays
  /// unique-per-host: the leader admits `B` in the declared domain and `C` without one; both commit at the
  /// majority and apply on every voter with the same domains.
  #[test]
  fn an_admission_carries_the_members_declared_domain() {
    let mut leader = council(OWNER);
    let mut follower = council(A);
    elect(&mut leader, &mut follower);
    assert!(leader.propose(Reconfiguration::Admit {
      host: B,
      domain: Some(DECLARED_DOMAIN),
    }));
    assert!(leader.propose(Reconfiguration::Admit {
      host: C,
      domain: None,
    }));
    replicate(&mut leader, &mut follower, 2);
    for (name, council) in [("leader", &leader), ("follower", &follower)] {
      let configuration = council.configuration();
      assert!(
        configuration.members.contains(&B) && configuration.members.contains(&C),
        "both admissions applied at the {name}"
      );
      assert_eq!(
        configuration.domains.get(&B),
        Some(&DECLARED_DOMAIN),
        "the declared domain rode the admission into the {name}'s configuration"
      );
      assert_eq!(
        configuration.domains.get(&C),
        None,
        "no declaration: the member stays unique-per-host at the {name}"
      );
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
      if let Some(message) = leader.replication_for(A, UNBOUNDED) {
        exchange(leader, follower, message);
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
      leader.propose(Reconfiguration::Admit {
        host: B,
        domain: None,
      }),
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

  /// AC (§4.8, D-14 — Raft Figure 2's follower timer-resets): **granting a real vote** advances the voter's
  /// leader-contact signal, so the drive loop's election timer resets and the voter defers its own campaign a
  /// full timeout — the candidate it just backed gets time to win and send its first heartbeat instead of
  /// being campaigned against (docs/bugs/2026-09-13-consensus-voters-outside-record-neighbourhood.md, sibling
  /// fixes). By use: after the election the follower has granted a real vote but answered no append yet, and
  /// its contact has advanced; the leader's first heartbeat then advances it again.
  #[test]
  fn granting_a_vote_defers_the_voters_own_election() {
    let mut leader = council(OWNER);
    let mut follower = council(A);
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

  /// AC (§4.8, D-14 — Raft Figure 2's follower timer-resets): an append from the **current** leader resets the
  /// election timer **even when the log-consistency check rejects it** — a follower still catching its log up
  /// has a live leader and must not campaign against it (the previous `success`-only gate left it aging while
  /// it defended that very leader). A stale-term append is not contact and does not reset. By use: a
  /// current-term append naming a previous entry the follower does not hold is answered `success: false`,
  /// yet the contact signal advances; an append at a stale term is refused without advancing it.
  #[test]
  fn a_current_leaders_rejected_append_still_resets_the_election_timer() {
    let mut leader = council(OWNER);
    let mut follower = council(A);
    elect(&mut leader, &mut follower);
    let before = follower.leader_contact();
    // Name a previous entry the follower cannot hold: rejected by the consistency check, yet live contact.
    let Some(RaftMessage::AppendEntries(mut rejected)) = leader.replication_for(A, UNBOUNDED)
    else {
      panic!("the leader owes its follower a heartbeat");
    };
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
    // A stale-term append (a deposed leader's) is refused and is not contact.
    let Some(RaftMessage::AppendEntries(mut stale)) = leader.replication_for(A, UNBOUNDED) else {
      panic!("the leader owes its follower a heartbeat");
    };
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

  /// AC (§4.8, D-14 — Raft §6.2 CheckQuorum, driven on the election-timeout cadence by the coordinator): a
  /// leader that hears from no majority across a whole window **steps down**, so a leader cut off from its
  /// followers does not sit on a term it cannot hold; one whose followers keep acknowledging stays. By use:
  /// the first tick after the election still counts the majority that elected it; a replication round's
  /// acknowledgement refreshes the window; a whole window with no acknowledgement steps the leader down.
  #[test]
  fn a_leader_that_hears_from_no_majority_steps_down_at_its_check_quorum_tick() {
    let mut leader = council(OWNER);
    let mut follower = council(A);
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
      "a follower's acknowledgement this window keeps it leader"
    );
    leader.check_quorum();
    assert!(
      !leader.is_leader(),
      "a whole window with no acknowledgement steps it down"
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
    replicate(&mut leader, &mut follower, 2); // commit the election no-op before reconfiguration
    assert!(leader.is_leader());

    // The region starts [OWNER, A]; host B has now joined the alive view, its node declaring a failure domain.
    let alive = vec![(OWNER, None), (A, None), (B, Some(DECLARED_DOMAIN))];
    assert!(
      !follower.reconcile_alive(&alive, &[]),
      "a non-leader proposes nothing — only the leader is the configuration master"
    );
    assert!(
      leader.reconcile_alive(&alive, &[]),
      "the leader proposes admitting the new member"
    );
    assert!(
      !leader.reconcile_alive(&alive, &[]),
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
    assert_eq!(
      follower.configuration().domains.get(&B),
      Some(&DECLARED_DOMAIN),
      "the admission carried B's declared failure domain to every voter (task #22)"
    );
    assert!(
      !leader.reconcile_alive(&alive, &[]),
      "the settled membership reconciles to a no-op — the log grows only for real changes"
    );
  }

  /// AC (§4.8, D-14; the suspicion window): the council retires a voter only once SWIM has **confirmed** its
  /// death, never while it is merely **suspected**. A single missed probe suspects a live voter; retiring it
  /// there discards a live member, and a later loss can then leave the survivors unable to reach a majority
  /// (`docs/bugs/2026-09-17-council-retires-a-suspected-voter.md` — the Linux whole-RAM regression). Two
  /// voters {OWNER, A}; A is absent from the alive view but its death is not confirmed: the leader proposes
  /// nothing and A stays a member. Once A is confirmed dead, the leader takes it over.
  #[test]
  fn a_suspected_voter_is_not_retired_until_its_death_is_confirmed() {
    let mut leader = council(OWNER);
    let mut follower = council(A);
    elect(&mut leader, &mut follower);
    replicate(&mut leader, &mut follower, 2); // commit the election no-op before reconfiguration
    assert!(leader.is_leader());

    // A is only SUSPECTED — absent from the alive view, but its death is not confirmed (an empty dead set).
    assert!(
      !leader.reconcile_alive(&[(OWNER, None)], &[]),
      "a suspected member is not retired — the suspicion window must resolve to death first"
    );
    assert!(
      leader.configuration().members.contains(&A),
      "the suspected member stays in the configuration"
    );

    // A is now CONFIRMED dead: the leader takes it over, and it retires.
    assert!(
      leader.reconcile_alive(&[(OWNER, None)], &[A]),
      "a confirmed-dead member is taken over"
    );
    replicate(&mut leader, &mut follower, 2);
    assert!(
      !leader.configuration().members.contains(&A),
      "the confirmed-dead member is retired"
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
    replicate(&mut leader, &mut follower, 2); // commit the election no-op before reconfiguration
    assert!(leader.is_leader());

    let a_epoch_before = leader
      .configuration()
      .epochs
      .get(&A)
      .copied()
      .expect("A is a member");

    // A is confirmed dead: the leader proposes its takeover (an epoch bump plus a retirement).
    assert!(
      leader.reconcile_alive(&[(OWNER, None)], &[A]),
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

  /// A council per node over `members` at f=1, every node seeded with the same voters
  /// ([`council_voters`]: the lowest ids up to the candidate floor of three).
  fn councils(members: &[HostId]) -> std::collections::BTreeMap<HostId, RegionalCouncil> {
    let quorum = Quorum { f: 1 };
    let voters = council_voters(&[], members, members, quorum);
    members
      .iter()
      .map(|&node| {
        (
          node,
          RegionalCouncil::new(
            node,
            members.to_vec(),
            voters.clone(),
            quorum,
            std::collections::BTreeMap::new(),
            3,
            false,
          ),
        )
      })
      .collect()
  }

  /// Elects `candidate` with the votes of the `reachable` peers (the full pre-vote then vote round): each
  /// request is broadcast to every reachable peer and every reply folded, until the exchange settles.
  fn elect_among(
    councils: &mut std::collections::BTreeMap<HostId, RegionalCouncil>,
    candidate: HostId,
    reachable: &[HostId],
  ) {
    let mut pending = councils
      .get_mut(&candidate)
      .map(RegionalCouncil::election_timeout)
      .unwrap_or_default();
    while let Some(request) = pending.pop() {
      // The requests are identical copies, one per voter: one broadcast serves them all.
      pending.clear();
      for &peer in reachable.iter().filter(|peer| **peer != candidate) {
        let reply = councils
          .get_mut(&peer)
          .and_then(|council| council.answer(request.clone()));
        if let Some(reply) = reply
          && let Some(council) = councils.get_mut(&candidate)
        {
          pending.extend(council.fold_reply(reply));
        }
      }
    }
  }

  /// One replication round from `leader` to each of its targets that is `reachable`: the peer answers the
  /// append and the leader folds the reply (round one carries the entry, round two the commit index).
  fn replicate_round(
    councils: &mut std::collections::BTreeMap<HostId, RegionalCouncil>,
    leader: HostId,
    reachable: &[HostId],
  ) {
    replicate_round_within(councils, leader, reachable, UNBOUNDED);
  }

  /// [`replicate_round`] with appends carrying at most `budget` entry bytes.
  fn replicate_round_within(
    councils: &mut std::collections::BTreeMap<HostId, RegionalCouncil>,
    leader: HostId,
    reachable: &[HostId],
    budget: usize,
  ) {
    let targets: Vec<HostId> = councils
      .get(&leader)
      .map(RegionalCouncil::replication_targets)
      .unwrap_or_default()
      .into_iter()
      .filter(|peer| *peer != leader && reachable.contains(peer))
      .collect();
    for peer in targets {
      let Some(message) = councils
        .get(&leader)
        .and_then(|l| l.replication_for(peer, budget))
      else {
        continue;
      };
      let reply = councils
        .get_mut(&peer)
        .and_then(|council| council.answer(message));
      if let Some(reply) = reply
        && let Some(council) = councils.get_mut(&leader)
      {
        council.fold_reply(reply);
      }
    }
  }

  /// `rounds` replication rounds from `leader` to its reachable targets.
  fn settle(
    councils: &mut std::collections::BTreeMap<HostId, RegionalCouncil>,
    leader: HostId,
    reachable: &[HostId],
    rounds: usize,
  ) {
    for _ in 0..rounds {
      replicate_round(councils, leader, reachable);
    }
  }

  /// What moving the voter set took, leader period by period: the periods spent catching added members up
  /// before the joint change began (thesis §4.2.1 — none when the change only removes voters), whether it
  /// began, whether it completed once its entry committed, and whether a further period found more to do.
  #[derive(Debug, PartialEq, Eq)]
  struct VoterChange {
    staging_periods: usize,
    began: bool,
    completed: bool,
    more: bool,
  }

  impl VoterChange {
    /// The joint change began, completed, and nothing more followed.
    fn moved_once(&self) -> bool {
      self.began && self.completed && !self.more
    }
  }

  /// Shape: the leader periods a test lets staging run before calling it stuck — a member one probe from
  /// caught up needs two (a refused probe that finds its end, then the batch), so four is ample.
  const STAGING_PERIODS_BOUND: usize = 4;

  /// Drives the voter set to follow the committed membership, leader period by period — stage and catch up
  /// any member the target adds, begin the joint change, complete it once its entry committed, then find
  /// nothing more to do — with the replication rounds each needs, the leader holding `alive` alive.
  fn drive_voter_change(
    councils: &mut std::collections::BTreeMap<HostId, RegionalCouncil>,
    leader: HostId,
    reachable: &[HostId],
    alive: &[HostId],
  ) -> VoterChange {
    let period = |councils: &mut std::collections::BTreeMap<HostId, RegionalCouncil>| {
      councils
        .get_mut(&leader)
        .is_some_and(|council| council.reconcile_voters(alive))
    };
    let mut staging_periods = 0;
    let began = loop {
      if period(councils) {
        break true;
      }
      if councils[&leader].raft.staged().is_empty() || staging_periods == STAGING_PERIODS_BOUND {
        break false;
      }
      staging_periods += 1;
      settle(councils, leader, reachable, 1);
    };
    settle(councils, leader, reachable, 1);
    let completed = period(councils);
    settle(councils, leader, reachable, 2);
    let more = period(councils);
    VoterChange {
      staging_periods,
      began,
      completed,
      more,
    }
  }

  /// AC (§4.8, D-14; Raft §6): a voter the council **retires** leaves the Raft voter set — it stops
  /// counting toward every majority — and the survivors commit alone under the new majority. Three voters
  /// {OWNER, A, B}; B dies; the leader takes B over (committed by OWNER and A), then moves the voter set:
  /// the joint change and `C_new` each commit with OWNER and A; afterwards `is_voter(B)` is false on both
  /// survivors and a further change commits with A's acknowledgement alone. Before this the voter set was
  /// fixed at boot: `is_voter(B)` stayed true after the takeover and every later commit still needed two
  /// acknowledgements of {OWNER, A, B} — one of them the dead B's
  /// (docs/bugs/2026-09-13-raft-voter-set-never-shrinks.md).
  #[test]
  fn a_retired_voter_leaves_the_council_and_the_survivors_commit_alone() {
    let members = [OWNER, A, B];
    let mut councils = councils(&members);
    elect_among(&mut councils, OWNER, &members);
    assert!(councils[&OWNER].is_leader());
    settle(&mut councils, OWNER, &[OWNER, A, B], 2);

    // B dies: the leader takes it over, and the survivors commit the takeover (two of three). The takeover
    // alone leaves the voter set untouched — the voter change follows.
    let survivors = [OWNER, A];
    let alive: Vec<(HostId, Option<DomainId>)> =
      survivors.iter().map(|host| (*host, None)).collect();
    let proposed = councils
      .get_mut(&OWNER)
      .is_some_and(|leader| leader.reconcile_alive(&alive, &[B]));
    settle(&mut councils, OWNER, &survivors, 2);
    assert_eq!(
      (
        proposed,
        councils[&OWNER].configuration().members.contains(&B),
        councils[&OWNER].is_voter(B),
      ),
      (true, false, true),
      "the takeover was proposed, committed and retired B; B still votes until the voter change"
    );

    // The voter set follows the committed membership: the joint change, then C_new, each committed by
    // the two survivors; then nothing more to do.
    let change = drive_voter_change(&mut councils, OWNER, &survivors, &survivors);
    assert!(
      change.moved_once() && change.staging_periods == 0,
      "began the joint change at once (it adds no voter), completed it once committed, then settled: {change:?}"
    );
    assert_eq!(
      (
        councils[&OWNER].is_voter(B),
        councils[&A].is_voter(B),
        councils[&OWNER].voters(),
      ),
      (false, false, vec![OWNER, A]),
      "B left the voter set on both survivors"
    );

    // A further change commits with A's acknowledgement alone — a majority of the two remaining voters.
    let admitted = councils.get_mut(&OWNER).is_some_and(|leader| {
      leader.propose(Reconfiguration::Admit {
        host: C,
        domain: None,
      })
    });
    settle(&mut councils, OWNER, &survivors, 2);
    assert_eq!(
      (
        admitted,
        councils[&OWNER].configuration().members.contains(&C),
        councils[&A].configuration().members.contains(&C),
      ),
      (true, true, true),
      "the survivors commit and apply a further change alone"
    );
  }

  /// AC (§4.8, D-14; docs/bugs/2026-09-22-council-seats-follow-id-order-not-liveness.md): a replacement
  /// admitted while its dead predecessor is still a member takes **no live voter's seat**. The KIND lane's
  /// history, ids in its order (survivor < replacement < predecessor < leader): the owner's pod is killed, its
  /// replacement authenticates under the same certificate with a fresh id and is admitted before the old id's
  /// death has held for the confirmation window. No seat is free, so the voter set must not move; before the
  /// fix it moved to the three lowest ids — the dead predecessor kept its seat and the live leader lost its
  /// own, and the replacement, which can never observe its own predecessor's death, won the next election and
  /// never retired it, so no takeover was ever assigned. Once the predecessor's death is confirmed and it is
  /// taken over, its freed seat goes to the replacement and the leader keeps leading.
  #[test]
  fn a_replacement_admitted_before_its_predecessor_retires_takes_no_live_voters_seat() {
    const SURVIVOR: HostId = HostId(10);
    const REPLACEMENT: HostId = HostId(20);
    const PREDECESSOR: HostId = HostId(30);
    const LEADER: HostId = HostId(40);
    let members = [SURVIVOR, PREDECESSOR, LEADER];
    let mut councils = councils(&members);
    elect_among(&mut councils, LEADER, &members);
    settle(&mut councils, LEADER, &members, 2);
    assert!(councils[&LEADER].is_leader());

    // The predecessor dies; its replacement is admitted first — alive, while the death is not yet confirmed.
    let reachable = [SURVIVOR, LEADER];
    let alive = [SURVIVOR, LEADER, REPLACEMENT];
    let declared: Vec<(HostId, Option<DomainId>)> =
      alive.iter().map(|host| (*host, None)).collect();
    let admitted = councils
      .get_mut(&LEADER)
      .is_some_and(|leader| leader.reconcile_alive(&declared, &[]));
    settle(&mut councils, LEADER, &reachable, 2);
    let configured = &councils[&LEADER].configuration().members;
    assert_eq!(
      (
        admitted,
        configured.contains(&REPLACEMENT),
        configured.contains(&PREDECESSOR)
      ),
      (true, true, true),
      "the replacement is a member beside its unretired predecessor"
    );

    // No seat is free: every sitting voter is still a member, so the voter set stays where it is.
    assert_eq!(
      drive_voter_change(&mut councils, LEADER, &reachable, &alive),
      VoterChange {
        staging_periods: 0,
        began: false,
        completed: false,
        more: false
      },
      "admitting a member moves no voter while every seat is held"
    );
    assert_eq!(
      (
        councils[&LEADER].is_leader(),
        councils[&LEADER].is_voter(LEADER),
        councils[&SURVIVOR].is_voter(LEADER),
        councils[&LEADER].is_voter(REPLACEMENT),
      ),
      (true, true, true, false),
      "the live leader keeps its seat and its leadership; the replacement waits as a learner"
    );

    // The predecessor's death is confirmed: it is taken over, and its freed seat goes to the replacement.
    // The replacement is a fresh member that joined the group's state as a learner (the fetch does this in a
    // fleet), and the leader reaches it — it is alive — so it can be caught up before it votes (thesis
    // §4.2.1); a member the leader could not reach would never be promoted.
    let retired = councils
      .get_mut(&LEADER)
      .is_some_and(|leader| leader.reconcile_alive(&declared, &[PREDECESSOR]));
    settle(&mut councils, LEADER, &reachable, 2);
    let (saved, base) = councils[&LEADER].join_state().unwrap();
    let mut replacement = RegionalCouncil::learner(REPLACEMENT, Quorum { f: 1 }, 3, false);
    replacement.join_from(saved, base).unwrap();
    councils.insert(REPLACEMENT, replacement);
    let change = drive_voter_change(&mut councils, LEADER, &alive, &alive);
    assert!(
      retired && change.moved_once() && change.staging_periods > 0,
      "the takeover committed, then the replacement was caught up and the voter change began, completed \
       and settled: {change:?}"
    );
    assert_eq!(
      (
        councils[&LEADER].is_leader(),
        councils[&LEADER].voters(),
        councils[&SURVIVOR].is_voter(PREDECESSOR),
      ),
      (true, vec![SURVIVOR, REPLACEMENT, LEADER], false),
      "the replacement holds the predecessor's seat; the leader still leads; the dead id votes nowhere"
    );
  }

  /// AC (§4.8, D-14; docs/bugs/2026-09-22-council-seats-follow-id-order-not-liveness.md): a seat freed by a
  /// takeover goes only to a member the leader holds **alive** — never to one it merely suspects, whatever
  /// its id. Four members at f=1: voters {OWNER, A, B}, learner C; B dies while C is suspected (in neither the
  /// alive nor the dead set). The takeover frees B's seat and the voter set shrinks to the two live voters
  /// rather than promoting C; once C's suspicion is refuted and it is alive again, it takes the free seat.
  #[test]
  fn a_free_seat_waits_for_a_member_the_leader_holds_alive() {
    let members = [OWNER, A, B, C];
    let mut councils = councils(&members);
    elect_among(&mut councils, OWNER, &[OWNER, A, B]);
    settle(&mut councils, OWNER, &[OWNER, A, B], 2);
    let survivors = [OWNER, A];
    let declared: Vec<(HostId, Option<DomainId>)> =
      survivors.iter().map(|host| (*host, None)).collect();
    let taken_over = councils
      .get_mut(&OWNER)
      .is_some_and(|leader| leader.reconcile_alive(&declared, &[B]));
    settle(&mut councils, OWNER, &survivors, 2);
    let change = drive_voter_change(&mut councils, OWNER, &survivors, &survivors);
    assert!(
      taken_over && change.moved_once() && change.staging_periods == 0,
      "B's takeover committed and the voter set moved once, adding no one: {change:?}"
    );
    assert_eq!(
      (councils[&OWNER].voters(), councils[&A].is_voter(C)),
      (vec![OWNER, A], false),
      "the freed seat stays empty while C is only suspected"
    );

    // C's suspicion is refuted: the leader holds it alive — and reaches it, which catching it up before it
    // votes requires (thesis §4.2.1) — so it takes the free seat.
    let alive = [OWNER, A, C];
    let change = drive_voter_change(&mut councils, OWNER, &alive, &alive);
    assert!(
      change.moved_once() && change.staging_periods > 0,
      "the live learner was caught up, then promoted to the free seat: {change:?}"
    );
    assert_eq!(councils[&OWNER].voters(), vec![OWNER, A, C]);
  }

  /// AC (§4.8, D-14 — "a small elected council"): beyond the candidate floor the extra members are learners;
  /// when a voter dies a learner the leader holds alive is **promoted** to voter in its place, so the council
  /// keeps tolerating `f` failures — and the promoted learner, which had *adopted* a fetched configuration
  /// as learners do, re-folds the committed log from the formed base, so its configuration equals the
  /// voters' exactly (members, version and the dead voter's fencing epoch, bumped once, not twice). Four
  /// members at f=1: voters {OWNER, A, B}, learner C; B dies; C becomes a voter.
  #[test]
  fn a_learner_is_promoted_when_a_voter_dies() {
    let members = [OWNER, A, B, C];
    let mut councils = councils(&members);
    let learner_at_boot = !councils[&C].is_voter(C);
    elect_among(&mut councils, OWNER, &[OWNER, A, B]);
    assert!(councils[&OWNER].is_leader());
    settle(&mut councils, OWNER, &[OWNER, A, B], 2);

    // B dies; the leader takes it over, committed by OWNER and A. The learner then fetches and adopts the
    // committed configuration, as the fleet's learner path does.
    let alive = [OWNER, A, C];
    let declared: Vec<(HostId, Option<DomainId>)> =
      alive.iter().map(|host| (*host, None)).collect();
    let proposed = councils
      .get_mut(&OWNER)
      .is_some_and(|leader| leader.reconcile_alive(&declared, &[B]));
    settle(&mut councils, OWNER, &alive, 2);
    let fetched = councils[&OWNER].configuration().clone();
    let adopted = councils
      .get_mut(&C)
      .is_some_and(|learner| learner.adopt(fetched));
    assert_eq!(
      (learner_at_boot, proposed, adopted),
      (true, true, true),
      "C was a learner beyond the candidate floor; B's takeover committed; C adopted the fetched configuration"
    );

    // The voter set follows: C is the one live learner, so it is caught up and promoted to B's freed seat.
    let change = drive_voter_change(&mut councils, OWNER, &alive, &alive);
    assert!(
      change.moved_once() && change.staging_periods > 0,
      "{change:?}"
    );
    assert_eq!(
      (
        councils[&OWNER].is_voter(C),
        councils[&C].is_voter(C),
        councils[&OWNER].is_voter(B),
      ),
      (true, true, false),
      "C is a voter now on the leader and on itself; the dead B is not"
    );
    assert_eq!(
      councils[&C].configuration(),
      councils[&OWNER].configuration(),
      "the promoted learner re-folded the log from the base: its configuration is the leader's, exactly"
    );
  }

  /// Shape: an append budget of about two council commands (an admission is 10 command bytes, 23 on the
  /// wire), so these tests cross batch boundaries as the fleet's budget does on a longer log.
  const SMALL_BUDGET: usize = 64;
  /// Shape: the membership changes the compaction tests commit — far past the first compaction of a
  /// three-member council (its configuration encodes to a few hundred bytes; each change is about thirty).
  const CHANGES: u64 = 120;

  /// The `index`-th change of a compaction test: admit a transient member, or retire it again, so the
  /// configuration's size stays level while its version and log keep growing.
  fn change(index: u64) -> Reconfiguration {
    let host = HostId(1_000 + index / 2);
    if index.is_multiple_of(2) {
      Reconfiguration::Admit { host, domain: None }
    } else {
      Reconfiguration::Retire(host)
    }
  }

  /// Applies `change` to an oracle configuration directly, as a committed command applies it.
  fn apply_to_oracle(oracle: &mut RegionalConfiguration, change: &Reconfiguration) {
    match change {
      Reconfiguration::Admit { host, domain } => {
        oracle.admit(*host, *domain, 3);
      }
      Reconfiguration::Retire(host) => {
        oracle.retire(*host, 3);
      }
      Reconfiguration::TakeOver(host) => {
        oracle.take_over(*host, 3);
      }
    }
  }

  /// Commits `changes` one at a time from `leader` to the `reachable` peers (each proposed once the last
  /// has committed, as `reconcile_alive` gates it), with the small append budget, and returns the oracle
  /// configuration the changes produce from `oracle`.
  fn commit_changes(
    councils: &mut std::collections::BTreeMap<HostId, RegionalCouncil>,
    leader: HostId,
    reachable: &[HostId],
    changes: std::ops::Range<u64>,
    mut oracle: RegionalConfiguration,
  ) -> RegionalConfiguration {
    for index in changes {
      let proposal = change(index);
      assert!(
        councils.get_mut(&leader).unwrap().propose(proposal.clone()),
        "change {index} proposed"
      );
      apply_to_oracle(&mut oracle, &proposal);
      for _ in 0..4 {
        replicate_round_within(councils, leader, reachable, SMALL_BUDGET);
      }
      assert!(councils[&leader].caught_up(), "change {index} committed");
    }
    oracle
  }

  /// The wire bytes of a retained log's entries.
  fn log_bytes(saved: &SavedRaft) -> usize {
    saved
      .log
      .iter()
      .map(crate::raft::LogEntry::encoded_len)
      .sum()
  }

  /// Thesis §5.1.2 (`crate::fold`): a council that compacts by the size rule folds exactly the configuration
  /// its committed changes produce (an oracle applying them directly), on every voter; and its retained log
  /// never holds more wire bytes than twice its last snapshot (the rule, plus the entries a leader keeps for
  /// a follower still taking them), where before 2026-09-28 it held every entry of the fleet's life.
  /// Non-vacuity: every voter compacted — the third included, which a leader compacting before its followers
  /// held the entries sent a snapshot instead each time.
  #[test]
  fn a_compacting_council_folds_what_its_changes_produce_and_stays_bounded() {
    let members = [OWNER, A, B];
    let mut councils = councils(&members);
    elect_among(&mut councils, OWNER, &members);
    settle(&mut councils, OWNER, &members, 2);
    let oracle = councils[&OWNER].configuration().clone();
    let oracle = commit_changes(&mut councils, OWNER, &members, 0..CHANGES, oracle);
    settle(&mut councils, OWNER, &members, 2);
    for (id, council) in &councils {
      assert_eq!(council.configuration(), &oracle, "{id:?} folds the oracle");
      assert!(council.compactions() > 0, "{id:?} compacted");
      let (saved, _) = council.join_state().unwrap();
      assert!(
        log_bytes(&saved) <= 2 * saved.snapshot_data.len(),
        "{id:?} retains {} log bytes past a snapshot of {} after {} compactions",
        log_bytes(&saved),
        saved.snapshot_data.len(),
        council.compactions()
      );
      assert!(
        saved.log.len() < usize::try_from(CHANGES).unwrap(),
        "{id:?} retains {} of {} entries",
        saved.log.len(),
        CHANGES
      );
    }
  }

  /// Raft §7 through the council: a voter cut off while the others commit past their compaction is sent the
  /// leader's snapshot when it returns — its missing entries are gone from the leader's log — installs it,
  /// takes the entries after it, and ends with exactly the leader's configuration.
  #[test]
  fn a_voter_left_behind_the_leaders_snapshot_catches_up_to_its_configuration() {
    let members = [OWNER, A, B];
    let mut councils = councils(&members);
    elect_among(&mut councils, OWNER, &members);
    settle(&mut councils, OWNER, &members, 2);
    let oracle = councils[&OWNER].configuration().clone();
    let oracle = commit_changes(&mut councils, OWNER, &[OWNER, A], 0..CHANGES, oracle);
    assert!(
      councils[&OWNER].snapshot_index() > councils[&B].last_log_index(),
      "the leader compacted past everything B holds"
    );
    let owed = councils[&OWNER].replication_for(B, SMALL_BUDGET);
    assert!(
      matches!(owed, Some(RaftMessage::InstallSnapshot(_))),
      "B is owed the snapshot, not an append"
    );
    // Round one ships the snapshot; the rounds after it the entries above it, a small batch each.
    let mut rounds = 0;
    while councils[&B].configuration() != councils[&OWNER].configuration() {
      replicate_round_within(&mut councils, OWNER, &members, SMALL_BUDGET);
      rounds += 1;
      assert!(
        rounds <= CHANGES,
        "B catches up in a bounded number of rounds"
      );
    }
    assert!(councils[&B].snapshot_index() > 0, "B installed a snapshot");
    assert_eq!(councils[&B].configuration(), &oracle);
    assert_eq!(
      councils[&B].configuration(),
      councils[&OWNER].configuration()
    );
  }

  /// A compacted council's state restores under its own identity and joins a fresh member to the same
  /// configuration; a snapshot paired with any other base is refused before anything is installed.
  #[test]
  fn a_compacted_council_restores_and_joins_and_refuses_a_mismatched_base() {
    let members = [OWNER, A, B];
    let mut councils = councils(&members);
    elect_among(&mut councils, OWNER, &members);
    settle(&mut councils, OWNER, &members, 2);
    let formed = councils[&OWNER].configuration().clone();
    commit_changes(&mut councils, OWNER, &members, 0..CHANGES, formed.clone());
    let (saved, base) = councils[&A].join_state().unwrap();
    assert!(saved.snapshot_index > 0, "the donor compacted");
    let expected = councils[&A].configuration().clone();

    let mut restored = council(A);
    restored.restore_from(saved.clone(), base.clone()).unwrap();
    assert_eq!(restored.configuration(), &expected);

    let mut joined = RegionalCouncil::learner(HostId(9), Quorum { f: 1 }, 3, false);
    joined.join_from(saved.clone(), base).unwrap();
    assert_eq!(joined.configuration(), &expected);

    let mut refused = RegionalCouncil::learner(HostId(10), Quorum { f: 1 }, 3, false);
    assert_eq!(
      refused.join_from(saved, formed),
      Err(RaftRecoveryError::InvalidSnapshot)
    );
    assert!(!refused.initialized());
  }

  /// Measurement (`docs/wip/BENCHMARKS.md`, "consensus log compaction"): a sole-voter council commits
  /// `changes` membership changes — each one a propose, commit and apply, the path a leader period runs — and
  /// reports the retained publication's size (the `SavedRaft` the control shard re-encodes before every
  /// reply), its log length, the compactions, and the time the history took. Run with
  /// `cargo test --release -p slates-cluster --lib measure_a_long_council_history -- --ignored --nocapture`.
  #[test]
  #[ignore = "a measurement, run by hand in release"]
  fn measure_a_long_council_history() {
    use slates_wire::Wire;
    for changes in [250u64, 1_000, 4_000] {
      let mut council = RegionalCouncil::new(
        OWNER,
        vec![OWNER],
        vec![OWNER],
        Quorum { f: 0 },
        std::collections::BTreeMap::new(),
        3,
        false,
      );
      let started = std::time::Instant::now();
      for index in 0..changes {
        assert!(council.propose(change(index)));
      }
      let took = started.elapsed();
      let (saved, _) = council.join_state().unwrap();
      eprintln!(
        "changes {changes}: retained {} bytes, log {} entries, compactions {}, {:.3} ms ({:.2} us per change)",
        saved.to_bytes().len(),
        saved.log.len(),
        council.compactions(),
        took.as_secs_f64() * 1e3,
        took.as_secs_f64() * 1e6 / changes as f64
      );
    }
  }
}
