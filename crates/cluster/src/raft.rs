//! The hecate Raft dialect, pure core (§4.8 mechanism 2 "Configuration, by consensus") — the regional
//! configuration group agrees membership, neighbourhoods, host epochs and takeover assignments by Raft,
//! not by the data-plane fenced register (that is mechanism 1). Built here: the **sans-io role and term
//! state machine with leader election** (Raft §5.2, §5.4.1 the election restriction) and **log
//! replication** (§5.3 `AppendEntries` — the consistency check, conflict truncation and log repair — and
//! §5.4.2 the commit-safety rule, so an earlier-term entry is never committed by replica count alone). A
//! deterministic state machine driven by an externally-timed `start_election`/`append_command` and by
//! received messages, so it is oracle-tested at N=1 before any timer or datagram is involved. The caller
//! owns the election and heartbeat timers and ships the [`RequestVote`]/[`VoteReply`]/[`AppendEntries`]/
//! [`AppendReply`] it returns.
//!
//! Also built: **PreVote** (§9.6) — a would-be candidate first runs a non-binding pre-vote round at the
//! term it *would* seek, without incrementing its own term; only on a majority of pre-votes does it start
//! a real election. A peer refuses a pre-vote while it still believes a leader is alive, so a
//! partitioned, term-inflated node cannot force a healthy leader to step down when it rejoins. And
//! **CheckQuorum** (§6.2) — a leader that has not been in contact with a majority since its previous
//! check steps down, so a leader cut off from the cluster stops acting as one. And **ReadIndex** (§6.4) —
//! the leader serves a linearizable read at its commit index without appending a log entry, safe only
//! when it has committed in its current term and is confirmed in contact with a majority. PreVote and
//! CheckQuorum together give the stability etcd's raft ships by default. And the **joint-consensus
//! majority rule** (§6) — during a membership change the node enters a joint configuration where every
//! decision needs a majority of *both* the old and new voter sets, so no two disjoint majorities can form
//! across the change; every quorum check (election, commit, CheckQuorum, ReadIndex) honours it.
//!
//! And **snapshot/log compaction with install-snapshot** (§7): [`compact`](RaftNode::compact) folds the
//! committed prefix into a snapshot and discards it, so the log stays bounded (every index resolves
//! through a snapshot offset that is a no-op until the first compaction); and a follower that has fallen
//! below the leader's snapshot — which no append can reach — is caught up by
//! [`install_snapshot_for`](RaftNode::install_snapshot_for)/[`on_install_snapshot`](RaftNode::on_install_snapshot),
//! its reply stating what it now holds (never the leader's own snapshot index, which may have moved on).
//! The groups compact by the thesis's size rule (`crate::fold`).
//!
//! **Replication is bounded and backs up by term** (2026-09-28): an append carries at most a byte budget
//! of entries (`raft_wire::append_batch_bytes`, a fresh session's first credit) and never nothing while one
//! is owed; a consistency-check refusal carries the conflict hint of §5.3 (the follower's conflicting term
//! and where its run begins, or where its log ends), so the leader backs up a whole term — or straight to
//! an empty follower's end, thesis §4.2.1 — in one round trip; progress only grows within a term, so a late
//! reply never moves it back; and an append anchored inside a follower's committed prefix is taken from the
//! follower's commit index on, never refused
//! (`docs/bugs/2026-09-28-a-late-append-could-land-compacted-entries-on-a-log.md`).
//!
//! And **learners with catch-up rounds** (thesis §4.2.1): a member a voter set adds is staged —
//! replicated to in rounds, each carrying what the leader held when it began, counted toward no quorum —
//! and [`catch_up`](RaftNode::catch_up) reports it ready once a round completes within one CheckQuorum
//! window (under an election timeout); a member whose lag does not shrink for a whole window is aborted
//! ("unavailable or so slow that it will never catch up"). Staging is leader-local and ends with
//! leadership. Replaying the thesis's Figure 4.4(a) — a fourth voter with an empty log, then the loss of
//! an original voter — the group could not commit for 21 replication rounds when the newcomer was added
//! directly, and committed in the first round when it was staged
//! (`a_staged_newcomer_leaves_no_availability_gap_where_a_direct_one_does`).
//!
//! And **priority** (`docs/wip/research/consensus-enhancements.md` §3.4): each voter's priority is its
//! quorum round trip — the round trip it would commit in as leader — measured by its caller
//! ([`set_priority`](RaftNode::set_priority)). A follower reports its own in every [`AppendReply`], and the
//! leader returns every voter's in its [`AppendEntries`], so each follower ranks itself against the same
//! table ([`election_rank`](RaftNode::election_rank); the caller's timer yields one timeout per rank). A
//! leader hands off to a voter that outranks it once it has led a whole window, at most once per leadership
//! that aborts ([`priority_transfer`](RaftNode::priority_transfer)). Overlapping intervals — round trip ±
//! spread — tie, so one host's noise never ranks anyone. On Microsoft's published inter-region matrix, five
//! regions: the fastest-committing region led every seed (14 of 20 before), and it took leadership back on
//! every seed after it returned from an outage (none before).
//! The multi-node **conformance suite** (`tests/raft.rs`) drives a cluster through election, replication,
//! a partition and a membership change, checking Election Safety, Log Matching, Leader Completeness and
//! State Machine Safety.
//!
//! And the **log-integrated membership change** (§6): [`begin`](RaftNode::begin_membership_change) and
//! [`complete_membership_change`](RaftNode::complete_membership_change) append `C_old,new`/`C_new`
//! configuration entries that take effect the moment they are appended (the effective configuration is
//! derived from the log, so a truncated entry reverts it) and replicate like any entry; compaction folds
//! a discarded configuration into the base, and install-snapshot carries it, so it is never lost. One
//! change is in flight at a time (a configuration entry is appended only once the previous one has
//! committed — Ongaro's thesis §4.1, the rule that keeps two changes from producing disjoint majorities);
//! a leader whose own removal commits **steps down** (§4.2.2); a node outside its effective configuration
//! never campaigns; and the leader keeps replicating to the **outgoing** voters until the entry that
//! removes them commits, so a live member demoted to learner learns its own removal
//! ([`replication_targets`](RaftNode::replication_targets)). This completes the dialect's core.
//!
//! Degenerate on a laptop (`f = 0`): one voter, itself; a pre-vote and an election each reach a majority
//! of one at once, an appended entry commits at once, and the lone voter is always its own quorum so it
//! never steps down — the same code path as a fleet, never a mode switch (R8). Owed: driving the dialect
//! live over the transport in a multi-node fleet (this core is sans-io and multi-node-tested by direct
//! message passing), and the PreVote/CheckQuorum timer cadence, which is the caller's clock.
//!
//! Evidence: Ongaro & Ousterhout, *In Search of an Understandable Consensus Algorithm (Extended
//! Version)*, 2014 (tier A); the safety argument for the election restriction is §5.4.

use std::collections::{BTreeMap, BTreeSet};

use slates_db::register::HostId;

/// A node's role in its term (Raft §5.1, plus the PreVote pre-candidacy of §9.6). A follower defers to a
/// leader; a pre-candidate is testing whether an election could win without yet inflating its term; a
/// candidate is seeking votes; a leader has a majority for its term.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
  /// Passive — grants votes and accepts a leader's entries.
  Follower,
  /// Running a pre-vote round (§9.6): gathering non-binding assurances that an election could win,
  /// without incrementing its term, so a partitioned node cannot disrupt a healthy leader.
  PreCandidate,
  /// Seeking votes for its term.
  Candidate,
  /// Won a majority for its term.
  Leader,
}

/// A request for votes (Raft `RequestVote`): the candidate's term and the summary of its log the
/// election restriction (§5.4.1) compares against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestVote {
  /// The candidate's term.
  pub term: u64,
  /// The candidate seeking the vote.
  pub candidate: HostId,
  /// The index of the candidate's last log entry (zero when its log is empty).
  pub last_log_index: u64,
  /// The term of the candidate's last log entry (zero when its log is empty).
  pub last_log_term: u64,
}

/// A pre-vote request (Raft §9.6): asked at the term the candidate *would* seek (`current + 1`) without
/// the candidate incrementing its own term. A peer answers whether it would grant a real vote — but its
/// term is left untouched, so a partitioned high-term node cannot force the cluster's term upward.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreVote {
  /// The term the candidate would seek (its current term plus one).
  pub term: u64,
  /// The pre-candidate.
  pub candidate: HostId,
  /// The index of the candidate's last log entry.
  pub last_log_index: u64,
  /// The term of the candidate's last log entry.
  pub last_log_term: u64,
}

/// A reply to a [`PreVote`]: the voter and whether it would grant a real vote. It carries no term
/// authority — a pre-vote never changes any node's term.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreVoteReply {
  /// The voter replying.
  pub voter: HostId,
  /// The term the pre-vote was for (the candidate matches replies to its pre-election).
  pub term: u64,
  /// Whether the voter would grant a real vote.
  pub granted: bool,
}

/// A reply to a [`RequestVote`]: the replying voter, its current term (so a candidate learns of a newer
/// term), whether it granted the vote, and — when it did — its window slots above the candidate's last log
/// entry, which the candidate recovers from (`docs/wip/research/consensus-enhancements.md` §4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoteReply {
  /// The voter replying.
  pub voter: HostId,
  /// The voter's current term.
  pub term: u64,
  /// Whether the vote was granted.
  pub granted: bool,
  /// The voter's window slots above the candidate's last log entry (empty unless granted).
  pub reports: Vec<SlotReport>,
}

/// A proposer's command sent straight to every voter — the **fast track**
/// (`docs/wip/research/consensus-enhancements.md` §3.7): a voter synced to the term's leader votes it at
/// `index`, and the leader decides the index from the votes, committing in one round trip from the proposer
/// when a fast quorum voted the same command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FastPropose {
  /// The term the proposer knows.
  pub term: u64,
  /// The proposing node.
  pub proposer: HostId,
  /// The one-based index proposed at.
  pub index: u64,
  /// The command.
  pub command: Vec<u8>,
}

/// A voter's fast vote, sent to its term's leader: the command it holds at `index` for this term (its first
/// vote there stands; one vote per index per term).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FastVote {
  /// The voter's term.
  pub term: u64,
  /// The voter.
  pub voter: HostId,
  /// The one-based index voted at.
  pub index: u64,
  /// The command voted.
  pub command: Vec<u8>,
}

/// A leader's invitation to a caught-up voter to start an election at once (thesis §3.10, leadership
/// transfer): sent only once the target's log matches the leader's, so the target wins the election it
/// starts without waiting for its own timeout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeoutNow {
  /// The leader's term; a target at another term ignores the invitation.
  pub term: u64,
  /// The leader sending the invitation.
  pub leader: HostId,
}

/// Why a leadership transfer was refused (the closed taxonomy; thesis §3.10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferRefusal {
  /// Only the leader can hand leadership off.
  NotLeader,
  /// The target is the leader itself.
  TargetIsSelf,
  /// The target does not vote in the effective configuration, so it could not win.
  TargetNotVoter,
  /// A transfer to `target` is already in flight; one at a time.
  InFlight {
    /// The transfer's target.
    target: HostId,
  },
}

/// A leadership transfer in flight (thesis §3.10): its target, the term it was started in (it is void once
/// this node no longer leads that term), how many CheckQuorum ticks it has waited through, and whether the
/// invitation has gone out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Transfer {
  target: HostId,
  term: u64,
  quorum_checks: u8,
  invited: bool,
}

/// Derived: the CheckQuorum ticks a leadership transfer may wait through before the leader aborts it and
/// accepts proposals again (thesis §3.10: abort "after about an election timeout"). The tick runs once per
/// election timeout; a transfer started just before a tick has only a sliver of a timeout behind it at that
/// tick, so the abort comes at the second tick — at least one whole election timeout, at most two.
const TRANSFER_QUORUM_CHECKS: u8 = 2;

/// A member being caught up before it votes (thesis §4.2.1, "Catching up new servers"): the leader
/// replicates to it in rounds, each replicating everything the leader held when the round began, and the
/// member is caught up once a round completes within one CheckQuorum window — under an election timeout,
/// "the assumption that there are not enough unreplicated entries to create a significant availability
/// gap". Leader-local, never replicated: a new leader stages afresh.
#[derive(Clone, Copy, Debug)]
struct Staging {
  /// The leader's last index when the current round began: the round completes when the member matches it.
  round_end: u64,
  /// Whether a CheckQuorum tick has passed since the current round began.
  window_passed: bool,
  /// The member's lag behind the leader's last index at the previous tick (`u64::MAX` before the first).
  lag_at_tick: u64,
  /// Consecutive whole windows in which that lag did not shrink.
  strikes: u8,
  /// Whether a round has completed within one window.
  caught_up: bool,
}

/// Derived: the CheckQuorum ticks a new leader waits before handing leadership to a voter that outranks it
/// (§3.4): two — the first tick after an election may come a sliver after it, so the second is the first
/// to span a whole election timeout of leading, over which every voter's reply has refreshed its priority
/// in the leader's table.
const PRIORITY_WINDOWS: u8 = 2;

/// Derived: the whole CheckQuorum windows (election timeouts) a staged member's lag may go without
/// shrinking before its staging is aborted (thesis §4.2.1: "the leader should also abort the change if the
/// new server is unavailable or is so slow that it will never catch up"). One: a window holds about ten
/// replication rounds, and an available member takes at least a batch in each. The first tick after
/// staging begins only sets the baseline — the window before it may be a sliver — so the abort comes at the
/// second tick at the earliest, at least one whole election timeout after staging began, as a transfer's
/// does ([`TRANSFER_QUORUM_CHECKS`]).
const STALLED_WINDOWS: u8 = 1;

/// Where catching up the members a voter set adds stands (thesis §4.2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatchUp {
  /// Every member the target set adds is caught up, or it adds none: the membership change may begin.
  Ready,
  /// Members are still being caught up; the leader replicates to them in rounds.
  Pending,
  /// A staged member's lag did not shrink for [`STALLED_WINDOWS`] whole windows, so its staging ended. A
  /// later call stages it afresh ("the caller may always try again").
  Aborted {
    /// The member whose staging ended.
    member: HostId,
  },
  /// This node does not lead.
  NotLeader,
}

/// A voter configuration (Raft §6): the base voter set and, during a membership change, the incoming
/// set. A decision needs a majority of the base and — when `joint` is set — of the incoming set too.
#[derive(slates_wire::Wire, Clone, Debug, PartialEq, Eq)]
pub struct VoterConfig {
  /// The base voter set.
  pub voters: Vec<HostId>,
  /// The incoming voter set while a joint membership change is in flight.
  pub joint: Option<Vec<HostId>>,
}

/// One entry in the replicated log (Raft's per-entry term is the basis of the log-matching property).
/// A normal entry carries an opaque `command` the state machine applies (for the configuration group, an
/// encoded configuration change — the Raft core does not interpret it). A **configuration entry** instead
/// carries a [`VoterConfig`] that changes the Raft voter set; it takes effect the moment it is appended
/// (§6), so the Raft core reads it directly rather than through the state machine.
#[derive(slates_wire::Wire, Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
  /// The term in which the leader created this entry.
  pub term: u64,
  /// The command to apply once the entry commits (empty for a configuration entry).
  pub command: Vec<u8>,
  /// The voter configuration this entry installs, when it is a configuration entry (Raft §6).
  pub config: Option<VoterConfig>,
}

impl LogEntry {
  /// A normal command entry.
  pub fn command(term: u64, command: Vec<u8>) -> LogEntry {
    LogEntry {
      term,
      command,
      config: None,
    }
  }

  /// A configuration entry installing `config` (Raft §6, take-effect-on-append).
  pub fn configuration(term: u64, config: VoterConfig) -> LogEntry {
    LogEntry {
      term,
      command: Vec::new(),
      config: Some(config),
    }
  }

  /// The bytes this entry takes on the wire (`crate::raft_wire`, which a test holds to this count): the
  /// term, the command's length and bytes, the configuration's presence byte, and for a configuration
  /// entry its voter set (a count and eight bytes per voter) and the joint set's presence byte and set.
  pub fn encoded_len(&self) -> usize {
    let hosts =
      |set: &[HostId]| ENTRY_COUNT_BYTES.saturating_add(set.len().saturating_mul(size_of::<u64>()));
    let config = self.config.as_ref().map_or(0, |config| {
      hosts(&config.voters)
        .saturating_add(ENTRY_FLAG_BYTES)
        .saturating_add(config.joint.as_deref().map_or(0, hosts))
    });
    size_of::<u64>()
      .saturating_add(ENTRY_COUNT_BYTES)
      .saturating_add(self.command.len())
      .saturating_add(ENTRY_FLAG_BYTES)
      .saturating_add(config)
  }
}

/// Format: a length or count on the Raft wire is a little-endian `u32`.
const ENTRY_COUNT_BYTES: usize = size_of::<u32>();
/// Format: a presence flag on the Raft wire is one byte.
const ENTRY_FLAG_BYTES: usize = 1;
/// Format: the smallest entry's wire bytes — a term, an empty command's length, no configuration
/// ([`LogEntry::encoded_len`] of a no-op).
const MIN_ENTRY_BYTES: usize = size_of::<u64>() + ENTRY_COUNT_BYTES + ENTRY_FLAG_BYTES;

/// Derived: the fast quorum of `voters` — the smallest `f` with `2f + q > 2n` for the classic quorum
/// `q = ⌊n/2⌋ + 1`, the size at which a value a fast quorum accepted has the most votes in every classic
/// quorum (Lamport, *Fast Paxos*, 2006; Fast Raft's ⌈3n/4⌉, which it equals — `tests/slot_model.rs` asserts
/// it for every scope it runs).
pub fn fast_quorum(voters: usize) -> usize {
  let classic = voters / 2 + 1;
  (1..=voters)
    .find(|fast| {
      fast
        .saturating_mul(2)
        .saturating_add(classic)
        .saturating_sub(voters.saturating_mul(2))
        > 0
    })
    .unwrap_or(voters)
}

/// A voter's election priority (`docs/wip/research/consensus-enhancements.md` §3.4): the round trip it
/// would commit in as leader — its quorum round trip, the `⌊n/2⌋`-th smallest measured round trip to the
/// other voters (a leader commits once a majority including itself holds an entry) — and the spread of the
/// path that sets it, both in nanoseconds. A zero round trip is unknown (a needed path has no sample yet):
/// it never outranks and is never outranked, so an unmeasured group behaves as one without priorities.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ElectionPriority {
  /// The quorum round trip, nanoseconds; zero when unknown.
  pub quorum_ns: u64,
  /// The spread of the path that sets it (the estimator's `4 · rttvar`), nanoseconds.
  pub spread_ns: u64,
}

impl ElectionPriority {
  /// Whether this priority is known and commits distinguishably faster than `other`: its interval — the
  /// round trip plus its spread — lies wholly below `other`'s round trip less its spread. Overlapping
  /// intervals tie, so measurement noise never ranks one voter above another.
  pub fn outranks(&self, other: &ElectionPriority) -> bool {
    self.quorum_ns > 0
      && other.quorum_ns > 0
      && self.quorum_ns.saturating_add(self.spread_ns)
        < other.quorum_ns.saturating_sub(other.spread_ns)
  }
}

/// A slot in a node's **window** above its log (`docs/wip/research/consensus-enhancements.md` §4): the
/// leader's entry that arrived ahead of a hole in the log, or a fast vote for a proposer's entry. A node
/// accepts either only once it is synced to its term's leader, keeps it until it syncs to a newer term's
/// leader — so Raft's truncation of the log never erases a vote a slot records — and reports it with its
/// vote, for the new leader's recovery.
#[derive(slates_wire::Wire, Clone, Debug, PartialEq, Eq)]
pub struct WindowSlot {
  /// The term it was accepted in: the leader's, for its entry; the voter's, for a fast vote.
  pub term: u64,
  /// Whether it is a fast vote, rather than the leader's entry.
  pub fast: bool,
  /// The entry.
  pub entry: LogEntry,
}

impl WindowSlot {
  /// Its ballot: its term, and within a term the leader's entry outranks a fast vote.
  fn ballot(&self) -> (u64, bool) {
    (self.term, !self.fast)
  }
}

/// A window slot at its one-based log index: what a voter reports with its vote, and what a node retains.
#[derive(slates_wire::Wire, Clone, Debug, PartialEq, Eq)]
pub struct SlotReport {
  /// The slot's one-based log index.
  pub index: u64,
  /// The slot.
  pub slot: WindowSlot,
}

/// A leader's replication message (Raft `AppendEntries`): the leader's term, the log position it is
/// appending after (`prev_log_index`/`prev_log_term`, the consistency check), the entries to append
/// (empty for a heartbeat), and the leader's commit index. A follower appends only when its log matches
/// at the previous position, so the logs converge (the log-matching property, §5.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppendEntries {
  /// The current read-confirmation round, or zero when no read is pending. A follower echoes it
  /// only after recognizing this term's leader, so old replies cannot confirm a later read.
  pub read_context: u64,
  /// The leader's term.
  pub term: u64,
  /// The leader sending the entries.
  pub leader: HostId,
  /// The index immediately preceding the new entries (zero at the start of the log).
  pub prev_log_index: u64,
  /// The term of the entry at `prev_log_index` (zero at the start of the log).
  pub prev_log_term: u64,
  /// The entries to append (empty for a heartbeat).
  pub entries: Vec<LogEntry>,
  /// The leader's commit index, so the follower may advance its own.
  pub leader_commit: u64,
  /// Every voter's election priority as the leader last heard it, its own included (§3.4), so each
  /// follower ranks itself against the same table.
  pub priorities: Vec<(HostId, ElectionPriority)>,
  /// The index of the first entry this leader appended after its recovery — its **sync point** — or zero
  /// before it has appended one. A follower whose log holds the leader's entry there is synced to the
  /// leader's term, and drops its window slots of older terms.
  pub sync_index: u64,
  /// The first index open to the fast track this term, or zero when the leader proposes classically.
  pub open_from: u64,
}

/// A follower's reply to [`AppendEntries`]: the follower, its current term, whether the append
/// succeeded (the consistency check held), and — on success — the highest log index it now matches the
/// leader on, so the leader advances `match_index`/`next_index` for it. A consistency-check refusal
/// carries the conflict hint of Raft §5.3 (the term of the follower's entry at the leader's previous
/// index and the first index the follower holds for that term, or — when its log is too short — no term
/// and the index after its last entry), so the leader backs up past a whole conflicting term, or straight
/// to the follower's end, in one round trip instead of one entry per round trip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppendReply {
  /// The read context from the request this reply answers; zero is not a read confirmation.
  pub read_context: u64,
  /// The follower replying.
  pub follower: HostId,
  /// The follower's current term.
  pub term: u64,
  /// Whether the append succeeded.
  pub success: bool,
  /// On success, the last index the follower's log now matches the leader on.
  pub match_index: u64,
  /// On a consistency-check refusal, the term of the follower's entry at the previous index, or zero when
  /// the follower's log does not reach it. Zero otherwise.
  pub conflict_term: u64,
  /// On a consistency-check refusal, where the leader should look next: the first index the follower holds
  /// of `conflict_term`, or the index after the follower's last entry when `conflict_term` is zero. Zero
  /// otherwise.
  pub conflict_index: u64,
  /// The follower's own election priority, as it last measured it (§3.4).
  pub priority: ElectionPriority,
}

/// A leader's snapshot transfer (Raft `InstallSnapshot`, §7) — sent to a follower that has fallen below
/// the leader's snapshot, so `AppendEntries` cannot reach it (the entries it needs were compacted away).
/// It resets the follower's log to begin after the snapshot's last included entry and carries the
/// state-machine state at that point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallSnapshot {
  /// The leader's term.
  pub term: u64,
  /// The leader sending the snapshot.
  pub leader: HostId,
  /// The index of the last entry the snapshot includes (the follower's log resets to just after it).
  pub last_included_index: u64,
  /// The term of that last included entry (checked against any entry the follower still holds there).
  pub last_included_term: u64,
  /// The voter configuration in effect at the snapshot, so a follower that discards its log to install
  /// the snapshot does not lose it (Raft §6 configurations live in the state, hence the snapshot).
  pub config: VoterConfig,
  /// The state-machine state at the snapshot (opaque to the Raft core; the caller applies it).
  pub state: Vec<u8>,
}

/// A follower's reply to [`InstallSnapshot`]: the follower, its current term, and how far its log is now
/// known to match the leader's — its commit index once the snapshot is handled (committed entries are the
/// same on every server, and an installed snapshot is committed), or zero when it declined the snapshot
/// (its state did not decode), so the leader never credits a follower with entries it does not hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InstallSnapshotReply {
  /// The follower replying.
  pub follower: HostId,
  /// The follower's current term.
  pub term: u64,
  /// The index the follower's log now matches the leader's through, or zero when it declined.
  pub match_index: u64,
}

/// One outstanding ReadIndex round (§6.4): a read's start index and voter configuration, plus
/// only the distinct voters that have answered that round. One round bounds pending read state.
struct ReadRound {
  context: u64,
  term: u64,
  index: u64,
  config: VoterConfig,
  confirmed: BTreeSet<HostId>,
}

/// The complete state retained with a voter identity (§4.8; Raft §3.8/§5). Term and vote cannot
/// be recovered independently of the log, snapshot and configuration. The commit index is retained
/// too, so the group's deterministic fold can resume without exposing a shorter committed prefix.
#[derive(slates_wire::Wire, Clone, Debug, PartialEq, Eq)]
pub struct SavedRaft {
  /// The only voter identity this publication can recover.
  pub id: HostId,
  /// The configuration at the snapshot boundary, before the remaining log entries.
  pub base: VoterConfig,
  /// The greatest observed election term.
  pub term: u64,
  /// The vote already granted in that term.
  pub voted_for: Option<HostId>,
  /// All entries above the snapshot, including the uncommitted tail and configuration entries.
  pub log: Vec<LogEntry>,
  /// The last committed position.
  pub commit_index: u64,
  /// The last position folded into the state-machine snapshot.
  pub snapshot_index: u64,
  /// The term at that position, needed for log matching.
  pub snapshot_term: u64,
  /// The state-machine snapshot at that position.
  pub snapshot_data: Vec<u8>,
  /// The window above the log: every slot is an accepted value, retained as the log is, since recovery
  /// counts on it (`docs/wip/research/consensus-enhancements.md` §4).
  pub window: Vec<SlotReport>,
  /// The term of the leader this node's log was last synced to (zero before any).
  pub synced_term: u64,
}

/// A publication that cannot describe a legal recovered Raft state (§4.8). The caller refuses
/// recovery; none of these errors permits constructing a fresh voter under the saved identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RaftRecoveryError {
  /// Joining again would discard a term, vote or prefix this member has already observed.
  AlreadyInitialized,
  /// A join attempted to reuse the state donor's identity instead of a fresh member identity.
  ReusedIdentity,
  /// A voter set is empty or repeats an identity.
  InvalidVoters,
  /// The snapshot boundary disagrees with its term or carries state at index zero.
  InvalidSnapshot,
  /// A log entry's term decreases or exceeds the saved current term.
  InvalidLogTerm,
  /// The committed position is outside the retained snapshot and log.
  InvalidCommitIndex,
  /// A retained position cannot be represented by the protocol's index width.
  IndexOverflow,
  /// A window slot is at index zero, repeats an index, or was accepted in a term later than the saved
  /// current term; or the synced term is.
  InvalidWindow,
}

impl SavedRaft {
  /// Validates the state before any recovered node is exposed. Framing and checksum verification
  /// belong to the transport or publication reader, before this decoded value reaches the core.
  fn validate(&self) -> Result<(), RaftRecoveryError> {
    let valid_set = |voters: &[HostId]| {
      !voters.is_empty() && voters.iter().copied().collect::<BTreeSet<_>>().len() == voters.len()
    };
    let valid_config = |config: &VoterConfig| {
      valid_set(&config.voters) && config.joint.as_deref().is_none_or(valid_set)
    };
    if !valid_config(&self.base) {
      return Err(RaftRecoveryError::InvalidVoters);
    }
    if (self.snapshot_index == 0 && (self.snapshot_term != 0 || !self.snapshot_data.is_empty()))
      || (self.snapshot_index > 0 && self.snapshot_term == 0)
      || self.snapshot_term > self.term
    {
      return Err(RaftRecoveryError::InvalidSnapshot);
    }
    let last_index = self
      .snapshot_index
      .checked_add(u64::try_from(self.log.len()).map_err(|_| RaftRecoveryError::IndexOverflow)?)
      .ok_or(RaftRecoveryError::IndexOverflow)?;
    if self.commit_index < self.snapshot_index || self.commit_index > last_index {
      return Err(RaftRecoveryError::InvalidCommitIndex);
    }
    let mut previous_term = self.snapshot_term;
    for entry in &self.log {
      if entry.term == 0 || entry.term < previous_term || entry.term > self.term {
        return Err(RaftRecoveryError::InvalidLogTerm);
      }
      if entry
        .config
        .as_ref()
        .is_some_and(|config| !valid_config(config))
      {
        return Err(RaftRecoveryError::InvalidVoters);
      }
      previous_term = entry.term;
    }
    self.validate_window()
  }

  /// Validates the window: each slot at a distinct positive index, accepted no later than the current term,
  /// and the synced term no later either.
  fn validate_window(&self) -> Result<(), RaftRecoveryError> {
    let mut indices = BTreeSet::new();
    let valid = self.synced_term <= self.term
      && self.window.iter().all(|report| {
        report.index > 0 && report.slot.term <= self.term && indices.insert(report.index)
      });
    if valid {
      Ok(())
    } else {
      Err(RaftRecoveryError::InvalidWindow)
    }
  }
}

/// A Raft node's state: its identity, the voters it counts a majority against, the persistent term and
/// vote (Raft's `currentTerm`/`votedFor`), its role, the votes gathered this election, the replicated
/// `log` and how far it is committed, and — while leader — the per-follower `next_index`/`match_index`
/// replication progress.
pub struct RaftNode {
  /// Changed persistent state, cleared only after its owner publishes it (§4.8).
  retention_pending: bool,
  id: HostId,
  voters: Vec<HostId>,
  joint: Option<Vec<HostId>>,
  current_term: u64,
  voted_for: Option<HostId>,
  role: Role,
  votes: BTreeSet<HostId>,
  pre_votes: BTreeSet<HostId>,
  has_leader: bool,
  /// The leader this node currently believes in — a follower learns it from an accepted append; a leader is
  /// itself (via [`leader`](RaftNode::leader)). A **redirection hint** only (for an operator command that must
  /// reach the leader), never consulted for safety. `None` when campaigning, stepped down, or a leader that
  /// lost its quorum.
  leader_hint: Option<HostId>,
  contacts: BTreeSet<HostId>,
  read_context: u64,
  read_round: Option<ReadRound>,
  log: Vec<LogEntry>,
  /// The indices of the log's configuration entries, kept beside it so the configuration in force is found
  /// without a scan: a leader asks for it on every append, reply and vote, and a scan of a long log there
  /// made a proposal cost 20 ms at a 5,000-entry backlog (2026-09-29).
  config_entries: BTreeSet<u64>,
  commit_index: u64,
  next_index: BTreeMap<HostId, u64>,
  match_index: BTreeMap<HostId, u64>,
  /// While leading: the followers whose place is a guess — after this leader's election, a refusal, or a
  /// member's staging — sent one batch from `next_index` at a time until one is acknowledged (thesis
  /// §10.2.2's fallback; etcd's probe state). The others are pipelined ([`replicate_to`](Self::replicate_to)).
  probing: BTreeSet<HostId>,
  snapshot_index: u64,
  snapshot_term: u64,
  snapshot_data: Vec<u8>,
  /// The leadership transfer in flight, while leading (thesis §3.10).
  transfer: Option<Transfer>,
  /// Transfers this node started that were aborted at their deadline (the non-vacuity counter).
  transfers_aborted: u64,
  /// The members being caught up before they vote, while leading (thesis §4.2.1); bounded by the target
  /// voter set the caller passes ([`catch_up`](RaftNode::catch_up)).
  staging: BTreeMap<HostId, Staging>,
  /// The member whose staging ended since the last [`catch_up`](RaftNode::catch_up), reported there once.
  staging_aborted: Option<HostId>,
  /// Stagings aborted (the non-vacuity counter of the abort path).
  stagings_aborted: u64,
  /// This node's own election priority, as its caller last measured it (§3.4).
  own_priority: ElectionPriority,
  /// Every voter's priority: while leading, as each follower last replied it; while following, the
  /// leader's table from its last append. Bounded by the replication targets.
  priorities: BTreeMap<HostId, ElectionPriority>,
  /// CheckQuorum ticks since this node began leading (saturating).
  windows_led: u8,
  /// Whether the transfer in flight was started by priority, so its abort is recognized.
  priority_transfer_pending: bool,
  /// Whether a priority transfer aborted during this leadership: no further one is tried until the next,
  /// since a leader refuses proposals while a transfer is in flight.
  priority_transfer_failed: bool,
  /// ElectionPriority transfers started (the non-vacuity counter).
  priority_transfers: u64,
  /// The window above the log (`docs/wip/research/consensus-enhancements.md` §4): the leader's entries that
  /// arrived out of order and fast votes, by index. Retained; bounded by [`window_budget`](Self::window_budget)
  /// bytes.
  window: BTreeMap<u64, WindowSlot>,
  /// The term of the leader this node's log was last synced to (its log held that leader's entry at the
  /// leader's sync point); only that term's slots are accepted into the window. Retained.
  synced_term: u64,
  /// The wire bytes the window may hold — the caller's, the same as one append's batch
  /// (`raft_wire::append_batch_bytes`); zero, the default, holds none.
  window_budget: usize,
  /// While a candidate: the window slots each voter that granted this election reported.
  reports: BTreeMap<HostId, Vec<SlotReport>>,
  /// While leading: the index of the first entry appended after this leader's recovery (its sync point), or
  /// zero before one.
  sync_index: u64,
  /// While leading: the first index open to the fast track this term, or zero when proposing classically.
  open_from: u64,
  /// The fast track's opening as this node last knew it announced — by its own opening while leading, by its
  /// leader's appends while following — with the term it was announced in: `(term, first open index)`. It
  /// counts in that term only, so an opening can never outlive its term, whichever way the term or the role
  /// moves on (an election this node starts, or a timeout that ends its own leadership without a new term).
  open_announced: (u64, u64),
  /// While leading with the fast track open: the votes at each index above the commit index, by voter —
  /// undecided indices within the window's span above the log, and decided ones still counting toward a fast
  /// quorum.
  fast_votes: BTreeMap<u64, BTreeMap<HostId, Vec<u8>>>,
  /// While leading: indices a fast quorum chose above what it knows committed, which that reaches once every
  /// index below them is committed.
  fast_chosen: BTreeSet<u64>,
  /// While leading: the index through which this leader knows the log committed, its fast choices included —
  /// at least the commit index, which stays classic (`docs/wip/research/consensus-enhancements.md` §4). Zero
  /// when not leading.
  fast_through: u64,
  /// The highest index this node proposed at on the fast track this term.
  fast_proposed: u64,
  /// Recoveries that re-proposed a value found only in windows, holes they filled with no-ops, window slots
  /// pruned under a classic commit, and leader's entries buffered and later absorbed into the log (the
  /// non-vacuity counters of the window's paths).
  window_counters: WindowCounters,
}

/// What the window's paths did over a node's life (the non-vacuity counters the explorer reads).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WindowCounters {
  /// Values a recovery re-proposed from voters' windows.
  pub recovered: u64,
  /// Of those, values a fast quorum could have chosen (Fast Paxos's rule).
  pub recovered_fast_choices: u64,
  /// Of those, values past this node's own window's reach above its log, which only a voter whose window
  /// reaches further reported.
  pub recovered_beyond_reach: u64,
  /// Free indices a recovery filled with a no-op below the last value it re-proposed.
  pub holes_filled: u64,
  /// Window slots pruned once a classic commit covered their index.
  pub pruned: u64,
  /// The leader's entries this node buffered in its window, arrived ahead of a hole.
  pub buffered: u64,
  /// Buffered entries moved into the log once the hole below them filled.
  pub absorbed: u64,
  /// Indices this node decided as leader from fast votes.
  pub decided_from_votes: u64,
  /// Batches this node sent as leader ahead of a follower's acknowledgements (pipelined, §3.5).
  pub sent_ahead: u64,
  /// Of those, indices a fast quorum chose, committed in one round.
  pub fast_commits: u64,
}

impl RaftNode {
  /// A fresh node at term zero with an empty log. It is a non-voting learner when `voters` does
  /// not contain its id; only a replicated membership change may then admit it (§4.8, AUD-07).
  /// The caller must never use this constructor to recreate a state-losing voter under its old id.
  pub fn new(id: HostId, voters: Vec<HostId>) -> RaftNode {
    RaftNode {
      retention_pending: true,
      id,
      voters,
      joint: None,
      current_term: 0,
      voted_for: None,
      role: Role::Follower,
      votes: BTreeSet::new(),
      pre_votes: BTreeSet::new(),
      has_leader: false,
      leader_hint: None,
      contacts: BTreeSet::new(),
      read_context: 0,
      read_round: None,
      log: Vec::new(),
      config_entries: BTreeSet::new(),
      commit_index: 0,
      next_index: BTreeMap::new(),
      match_index: BTreeMap::new(),
      probing: BTreeSet::new(),
      snapshot_index: 0,
      snapshot_term: 0,
      snapshot_data: Vec::new(),
      transfer: None,
      transfers_aborted: 0,
      staging: BTreeMap::new(),
      staging_aborted: None,
      stagings_aborted: 0,
      own_priority: ElectionPriority::default(),
      priorities: BTreeMap::new(),
      windows_led: 0,
      priority_transfer_pending: false,
      priority_transfer_failed: false,
      priority_transfers: 0,
      window: BTreeMap::new(),
      synced_term: 0,
      window_budget: 0,
      reports: BTreeMap::new(),
      sync_index: 0,
      open_from: 0,
      open_announced: (0, 0),
      fast_votes: BTreeMap::new(),
      fast_chosen: BTreeSet::new(),
      fast_through: 0,
      fast_proposed: 0,
      window_counters: WindowCounters::default(),
    }
  }

  /// The state to publish before acknowledging a changed term, vote, log or snapshot (§4.8).
  /// Volatile leadership, read rounds, contact evidence and replication progress are excluded.
  pub fn saved(&self) -> SavedRaft {
    SavedRaft {
      id: self.id,
      base: VoterConfig {
        voters: self.voters.clone(),
        joint: self.joint.clone(),
      },
      term: self.current_term,
      voted_for: self.voted_for,
      log: self.log.clone(),
      commit_index: self.commit_index,
      snapshot_index: self.snapshot_index,
      snapshot_term: self.snapshot_term,
      snapshot_data: self.snapshot_data.clone(),
      window: self
        .window
        .iter()
        .map(|(index, slot)| SlotReport {
          index: *index,
          slot: slot.clone(),
        })
        .collect(),
      synced_term: self.synced_term,
    }
  }

  /// Restores one validated publication as a follower, preserving its vote and committed prefix.
  /// Unlike reconstructing from a term and a log alone, this also restores compacted membership.
  pub fn restore(saved: SavedRaft) -> Result<RaftNode, RaftRecoveryError> {
    saved.validate()?;
    let mut node = RaftNode::new(saved.id, saved.base.voters);
    node.joint = saved.base.joint;
    node.current_term = saved.term;
    node.voted_for = saved.voted_for;
    node.snapshot_index = saved.snapshot_index;
    for entry in saved.log {
      node.push_entry(entry);
    }
    node.commit_index = saved.commit_index;
    node.snapshot_term = saved.snapshot_term;
    node.snapshot_data = saved.snapshot_data;
    node.window = saved
      .window
      .into_iter()
      .map(|report| (report.index, report.slot))
      .collect();
    node.synced_term = saved.synced_term;
    Ok(node)
  }

  /// Whether a reply would depend on state not yet published by the owner (§4.8).
  pub fn retention_pending(&self) -> bool {
    self.retention_pending
  }

  /// Marks the complete state published. Call only after the anchor publication succeeds.
  pub fn retained(&mut self) {
    self.retention_pending = false;
  }

  /// Drops volatile authority while an operator-reviewed replacement is being fetched (§4.8).
  /// The term, vote and log remain recoverable; the caller also stops elections and input RPCs.
  pub fn suspend(&mut self) {
    self.role = Role::Follower;
    self.has_leader = false;
    self.leader_hint = None;
    self.read_round = None;
    self.contacts.clear();
  }

  /// This node's id.
  pub fn id(&self) -> HostId {
    self.id
  }

  /// This node's role.
  pub fn role(&self) -> Role {
    self.role
  }

  /// This node's current term.
  pub fn term(&self) -> u64 {
    self.current_term
  }

  /// Whether this node is the leader for its term.
  pub fn is_leader(&self) -> bool {
    self.role == Role::Leader
  }

  /// The leader this node currently knows — itself when it leads, else the last leader it accepted an append
  /// from (`None` when campaigning, stepped down, or a leader that lost its quorum). A **redirection hint**
  /// only: an operator command that must reach the leader is forwarded here, and a stale hint costs a retry,
  /// never a safety violation (the target refuses if it is not in fact the leader). Never consulted on a
  /// safety path.
  pub fn leader(&self) -> Option<HostId> {
    if self.role == Role::Leader {
      Some(self.id)
    } else {
      self.leader_hint
    }
  }

  /// Who this node voted for in its current term, if anyone.
  pub fn voted_for(&self) -> Option<HostId> {
    self.voted_for
  }

  /// The caller's election timer fired with no leader contact: begin a **pre-election** (Raft §9.6).
  /// The node becomes a pre-candidate and forgets its belief in a leader, but does **not** increment its
  /// term; it returns the [`PreVote`] to send each other voter, asking whether a real election could
  /// win. A single voter's pre-vote already carries a majority, so it proceeds straight to a real
  /// election and leads (the `f = 0` degenerate, no messages). Preferring this over a direct
  /// `start_election` is what keeps a partitioned, term-inflated node from disrupting a healthy leader.
  /// A node that is **not a voter** of its effective configuration — removed by a committed membership
  /// change, or a learner that never was one — does not campaign at all (Ongaro's thesis §4.2.3: a
  /// removed server that kept campaigning would disrupt the cluster it no longer belongs to).
  pub fn on_election_timeout(&mut self) -> Vec<PreVote> {
    if !self.is_voter(self.id) {
      return Vec::new();
    }
    self.has_leader = false;
    self.leader_hint = None;
    self.role = Role::PreCandidate;
    self.pre_votes = BTreeSet::from([self.id]);
    if self.is_majority(&self.pre_votes) {
      self.start_election();
      return Vec::new();
    }
    let request = PreVote {
      term: self.current_term.saturating_add(1),
      candidate: self.id,
      last_log_index: self.last_log_index(),
      last_log_term: self.last_log_term(),
    };
    self
      .all_voters()
      .into_iter()
      .filter(|voter| *voter != self.id)
      .map(|_| request)
      .collect()
  }

  /// Answers a received [`PreVote`] (Raft §9.6) **without changing this node's term, vote or role** — a
  /// pre-vote is non-binding. The node would grant a real vote only if it does not currently believe a
  /// leader is alive (it has not heard from one since its own election timeout), it is not itself the
  /// leader, the pre-vote's term is ahead of its own, and the candidate's log is at least as up-to-date.
  /// Because the term is never touched, a partitioned node's inflated term cannot force a step-down here.
  pub fn on_pre_vote(&self, request: PreVote) -> PreVoteReply {
    let granted = self.is_voter(self.id)
      && !self.has_leader
      && self.role != Role::Leader
      && request.term > self.current_term
      && self.candidate_log_is_current(request.last_log_index, request.last_log_term);
    PreVoteReply {
      voter: self.id,
      term: request.term,
      granted,
    }
  }

  /// Handles a received [`PreVoteReply`]. While this node is a pre-candidate for this pre-term, a granted
  /// reply is counted; once a majority would grant, the node starts the **real** election (incrementing
  /// its term now, having confirmed it can win) and returns the [`RequestVote`] to send. Otherwise
  /// `None`.
  pub fn on_pre_vote_reply(&mut self, reply: PreVoteReply) -> Option<Vec<RequestVote>> {
    if self.role != Role::PreCandidate
      || reply.term != self.current_term.saturating_add(1)
      || !reply.granted
    {
      return None;
    }
    self.pre_votes.insert(reply.voter);
    if self.is_majority(&self.pre_votes) {
      return Some(self.start_election());
    }
    None
  }

  /// The caller's election timer fired: begin an election (Raft §5.2). Advance to the next term, become
  /// a candidate, vote for self, and return the [`RequestVote`] to send each *other* voter. A single
  /// voter reaches its own majority here and becomes leader with no messages (the `f = 0` degenerate).
  /// Prefer [`on_election_timeout`](RaftNode::on_election_timeout), which runs the pre-vote round first.
  pub fn start_election(&mut self) -> Vec<RequestVote> {
    if !self.is_voter(self.id) {
      return Vec::new();
    }
    self.retention_pending = true;
    self.current_term = self.current_term.saturating_add(1);
    self.read_round = None;
    self.role = Role::Candidate;
    self.voted_for = Some(self.id);
    self.votes = BTreeSet::from([self.id]);
    self.reports.clear();
    self.become_leader_if_majority();

    let request = RequestVote {
      term: self.current_term,
      candidate: self.id,
      last_log_index: self.last_log_index(),
      last_log_term: self.last_log_term(),
    };
    self
      .all_voters()
      .into_iter()
      .filter(|voter| *voter != self.id)
      .map(|_| request)
      .collect()
  }

  /// Handles a received [`RequestVote`] (Raft §5.2, §5.4.1). A request under a newer term first steps
  /// this node down to a follower at that term (clearing its vote). The vote is granted only when the
  /// request is for our current term, we have not already voted for someone else this term, and the
  /// candidate's log is at least as up-to-date as ours (the election restriction that keeps a leader's
  /// log a superset of every committed entry). Returns the reply to send back.
  pub fn on_request_vote(&mut self, request: RequestVote) -> VoteReply {
    if request.term > self.current_term {
      self.step_down(request.term);
    }
    let not_yet_voted_elsewhere =
      self.voted_for.is_none() || self.voted_for == Some(request.candidate);
    let granted = self.is_voter(self.id)
      && request.term == self.current_term
      && not_yet_voted_elsewhere
      && self.candidate_log_is_current(request.last_log_index, request.last_log_term);
    if granted {
      self.retention_pending |= self.voted_for != Some(request.candidate);
      self.voted_for = Some(request.candidate);
    }
    let reports = if granted {
      self.window_above(request.last_log_index)
    } else {
      Vec::new()
    };
    VoteReply {
      voter: self.id,
      term: self.current_term,
      granted,
      reports,
    }
  }

  /// This node's window slots above `index`, as a vote reports them.
  fn window_above(&self, index: u64) -> Vec<SlotReport> {
    self
      .window
      .range(index.saturating_add(1)..)
      .map(|(index, slot)| SlotReport {
        index: *index,
        slot: slot.clone(),
      })
      .collect()
  }

  /// Handles a received [`VoteReply`]. A reply carrying a newer term steps us down. Otherwise, while we
  /// are still the candidate for this term, a granted vote is counted, and reaching a majority makes us
  /// leader. A stale reply (an older term, or after we have moved on) is ignored.
  pub fn on_vote_reply(&mut self, reply: VoteReply) {
    if reply.term > self.current_term {
      self.step_down(reply.term);
      return;
    }
    if self.role == Role::Candidate && reply.term == self.current_term && reply.granted {
      self.votes.insert(reply.voter);
      self.reports.insert(reply.voter, reply.reports);
      self.become_leader_if_majority();
    }
  }

  /// Steps this node down to a follower at `term` on observing it from any message (Raft §5.1 "a node
  /// that sees a higher term becomes a follower"). A no-op if `term` is not newer.
  pub fn observe_term(&mut self, term: u64) {
    if term > self.current_term {
      self.step_down(term);
    }
  }

  /// Adopts `term` as the current term, reverting to a follower and forgetting this term's vote and any
  /// gathered votes.
  fn step_down(&mut self, term: u64) {
    self.retention_pending |= self.current_term != term || self.voted_for.is_some();
    self.current_term = term;
    self.voted_for = None;
    self.read_round = None;
    self.role = Role::Follower;
    self.leader_hint = None;
    self.votes.clear();
    self.reports.clear();
    self.transfer = None;
    self.staging.clear();
    self.sync_index = 0;
    self.open_from = 0;
    self.fast_votes.clear();
    self.fast_chosen.clear();
    self.fast_through = 0;
    self.fast_proposed = 0;
  }

  /// Becomes leader if the votes gathered this election are a majority of the voters, initialising the
  /// replication progress for each follower — `next_index` at the end of the leader's log (Raft's
  /// optimistic guess) and `match_index` at nothing known replicated (§5.3).
  fn become_leader_if_majority(&mut self) {
    if self.role != Role::Candidate || !self.is_majority(&self.votes) {
      return;
    }
    self.role = Role::Leader;
    self.recover();
    self.transfer = None;
    self.staging.clear();
    self.staging_aborted = None;
    self.windows_led = 0;
    self.priority_transfer_pending = false;
    self.priority_transfer_failed = false;
    // Start the CheckQuorum window already in contact with the voters that just elected it, so the first
    // check does not spuriously step a freshly-won leader down before its heartbeats have replied.
    self.contacts = self.votes.clone();
    let next = self.last_log_index().saturating_add(1);
    self.next_index.clear();
    self.match_index.clear();
    self.probing.clear();
    for peer in &self.all_voters() {
      if *peer != self.id {
        self.next_index.insert(*peer, next);
        self.match_index.insert(*peer, 0);
        self.probing.insert(*peer);
      }
    }
  }

  /// The recovery a new leader runs before its first append (`docs/wip/research/consensus-enhancements.md`
  /// §4; the design `tests/prefix_model.rs` verifies). Each index above its own last log entry that a
  /// window holds — its own, or one a granting voter reported — is decided by the highest ballot there: a
  /// leader's entry is re-proposed as it is; at a fast ballot, the value with at least `|Q| + |F| − n` of the
  /// reports (Fast Paxos's rule: the value a fast quorum could have chosen), and otherwise the index is
  /// free. The recovered values are appended at this term, with a
  /// no-op at each free index below the last of them, so this leader can commit them. It is then synced to
  /// its own term, and keeps its window: a slot goes only once a classic commit covers its index (§4), since
  /// until then a later leader's truncation can erase the log entries that carry it — clearing it here lost a
  /// chosen value (the explorer's seed 266, 2026-09-29). With empty windows — the classic path — it appends
  /// nothing.
  fn recover(&mut self) {
    let above = self.last_log_index();
    let mut reported: BTreeMap<u64, Vec<WindowSlot>> = BTreeMap::new();
    // Every slot above the log, as the prefix model's recovery reads them — not only those within this node's
    // own reach: a voter whose window reaches further may have helped choose a value there, and until
    // 2026-09-29 a leader with a smaller window left such an index free. Each reporter's window bounds what it
    // reports.
    let own = self
      .window
      .range(above.saturating_add(1)..)
      .map(|(index, slot)| (*index, slot.clone()));
    let voters = self
      .reports
      .values()
      .flatten()
      .filter(|report| report.index > above)
      .map(|report| (report.index, report.slot.clone()));
    for (index, slot) in own.chain(voters) {
      reported.entry(index).or_default().push(slot);
    }
    let heard = self.votes.len();
    let recovered: BTreeMap<u64, (LogEntry, bool)> = reported
      .iter()
      .filter_map(|(index, slots)| {
        self
          .recovered_value(slots, heard)
          .map(|found| (*index, found))
      })
      .collect();
    let reach = above.saturating_add(self.window_span());
    if let Some(&last) = recovered.keys().next_back() {
      for index in above.saturating_add(1)..=last {
        let entry = match recovered.get(&index) {
          Some((entry, fast)) => {
            self.window_counters.recovered = self.window_counters.recovered.saturating_add(1);
            if index > reach {
              self.window_counters.recovered_beyond_reach = self
                .window_counters
                .recovered_beyond_reach
                .saturating_add(1);
            }
            if *fast {
              self.window_counters.recovered_fast_choices = self
                .window_counters
                .recovered_fast_choices
                .saturating_add(1);
            }
            LogEntry {
              term: self.current_term,
              ..entry.clone()
            }
          }
          None => {
            self.window_counters.holes_filled = self.window_counters.holes_filled.saturating_add(1);
            LogEntry::command(self.current_term, Vec::new())
          }
        };
        self.push_entry(entry);
      }
    }
    self.reports.clear();
    self.synced_term = self.current_term;
    self.sync_index = 0;
    self.open_from = 0;
    self.fast_votes.clear();
    self.fast_chosen.clear();
    self.fast_through = 0;
    self.fast_proposed = 0;
    self.retention_pending = true;
  }

  /// What the recovery decides from the reports at one index, `slots`, heard from `heard` voters: the entry
  /// to re-propose and whether a fast ballot chose it, or `None` when the index is free.
  fn recovered_value(&self, slots: &[WindowSlot], heard: usize) -> Option<(LogEntry, bool)> {
    let highest = slots.iter().map(WindowSlot::ballot).max()?;
    let at_highest: Vec<&WindowSlot> = slots
      .iter()
      .filter(|slot| slot.ballot() == highest)
      .collect();
    let (_, decided) = highest;
    if decided {
      return at_highest.first().map(|slot| (slot.entry.clone(), false));
    }
    // Fast votes exist only in a term without a joint configuration (a membership change begins only with
    // the fast track closed, and on a committed log), so the configuration in force is the one they were
    // cast under; a joint one here is counted and treated as free.
    let config = self.effective_config();
    if config.joint.is_some() {
      return None;
    }
    let voters = config.voters.len();
    let threshold = heard
      .saturating_add(fast_quorum(voters))
      .saturating_sub(voters);
    at_highest
      .iter()
      .find(|candidate| {
        at_highest
          .iter()
          .filter(|other| other.entry == candidate.entry)
          .count()
          >= threshold
      })
      .map(|slot| (slot.entry.clone(), true))
  }

  /// The most indices above its log end a node's window may reach, and the recovery reads: the window
  /// budget over the smallest entry's wire bytes. Zero while the budget is zero, the default.
  fn window_span(&self) -> u64 {
    u64::try_from(self.window_budget / MIN_ENTRY_BYTES).unwrap_or(u64::MAX)
  }

  /// Every voter that participates now — the base set, plus the incoming set while a joint membership
  /// change is in flight (Raft §6). The leader sends votes and entries to all of them; a message's
  /// recipient is the connection, so duplicates in the union are harmless, but they are deduped here.
  pub fn all_voters(&self) -> Vec<HostId> {
    let config = self.effective_config();
    let mut set: BTreeSet<HostId> = config.voters.into_iter().collect();
    if let Some(new) = config.joint {
      set.extend(new);
    }
    set.into_iter().collect()
  }

  /// Whether `node` votes under the configuration in effect now — the base set, or either set of a joint
  /// change in flight. A removed voter stops being one the moment the entry that removes it is appended.
  pub fn is_voter(&self, node: HostId) -> bool {
    self.all_voters().contains(&node)
  }

  /// The peers the leader replicates to: every current voter ([`all_voters`](RaftNode::all_voters)),
  /// plus — while the latest configuration entry is still uncommitted — the **outgoing** voters of the
  /// configuration it replaces. An outgoing voter that is alive (a member demoted to learner because a
  /// lower-id member joined the council) thereby receives the entry that removes it in the same rounds
  /// that commit it, learns it is no longer a voter, and stops campaigning; a dead one costs nothing
  /// (there is no session to it). Bounded: the extra targets drop out the moment the change commits.
  pub fn replication_targets(&self) -> Vec<HostId> {
    let mut set: BTreeSet<HostId> = self.all_voters().into_iter().collect();
    if self.role == Role::Leader {
      set.extend(self.staging.keys().copied());
    }
    if self.latest_config_index() > self.commit_index
      && let Some(outgoing) = self.config_before_latest()
    {
      set.extend(outgoing.voters);
      if let Some(joint) = outgoing.joint {
        set.extend(joint);
      }
    }
    set.into_iter().collect()
  }

  /// The one-based log index of the most recent configuration entry, or zero when the log holds none
  /// (the effective configuration is then the base).
  fn latest_config_index(&self) -> u64 {
    self.config_entries.last().copied().unwrap_or(0)
  }

  /// The configuration the latest configuration entry replaced: the previous configuration entry in the
  /// log, or the base when it is the only one. `None` when the log holds no configuration entry.
  fn config_before_latest(&self) -> Option<VoterConfig> {
    let mut latest_first = self.config_entries.iter().rev();
    latest_first.next()?;
    Some(
      latest_first
        .next()
        .and_then(|index| self.config_at(*index))
        .unwrap_or_else(|| self.base_config()),
    )
  }

  /// The configuration in force **at the commit index**: the most recent configuration entry at or below
  /// it, or the base when none is committed — what a leader consults to learn that its own removal has
  /// committed.
  fn committed_config(&self) -> VoterConfig {
    self
      .config_entries
      .range(..=self.commit_index)
      .next_back()
      .and_then(|index| self.config_at(*index))
      .unwrap_or_else(|| self.base_config())
  }

  /// A leader whose own removal has **committed** steps down (Ongaro's thesis §4.2.2): once the sole
  /// configuration in force at the commit index no longer names it, it stops leading — it managed the
  /// cluster through the change and now hands over to the new voters, who elect among themselves. While
  /// the change is still joint it keeps leading (the old configuration still names it).
  fn step_down_if_removed(&mut self) {
    if self.role != Role::Leader {
      return;
    }
    let committed = self.committed_config();
    if committed.joint.is_none() && !committed.voters.contains(&self.id) {
      self.read_round = None;
      self.role = Role::Follower;
      self.has_leader = false;
      self.leader_hint = None;
      self.votes.clear();
    }
  }

  /// The voter configuration in effect now: the most recent configuration entry in the log (a
  /// configuration takes effect the moment it is appended, before it commits — Raft §6), or the base
  /// configuration when the log holds none. A truncated configuration entry reverts the effective
  /// configuration automatically, because it is derived from the log rather than stored.
  fn effective_config(&self) -> VoterConfig {
    self
      .config_entries
      .last()
      .and_then(|index| self.config_at(*index))
      .unwrap_or_else(|| self.base_config())
  }

  /// Whether `granters` form a majority under the current configuration (the quorum intersection Raft's
  /// safety rests on): more than half of the base voters, **and** — while a joint change is in flight —
  /// more than half of the incoming voters too, so no two disjoint majorities can form across the change.
  fn is_majority(&self, granters: &BTreeSet<HostId>) -> bool {
    let carries =
      |set: &[HostId]| set.iter().filter(|voter| granters.contains(voter)).count() > set.len() / 2;
    let config = self.effective_config();
    carries(&config.voters) && config.joint.as_ref().is_none_or(|new| carries(new))
  }

  /// Whether this node is in a joint configuration (a membership change is in flight).
  pub fn in_joint_configuration(&self) -> bool {
    self.effective_config().joint.is_some()
  }

  /// Whether a candidate's last-log summary is at least as up-to-date as ours (Raft §5.4.1): a later
  /// last term wins; at an equal last term the longer (or equal) log wins.
  fn candidate_log_is_current(&self, candidate_index: u64, candidate_term: u64) -> bool {
    candidate_term > self.last_log_term()
      || (candidate_term == self.last_log_term() && candidate_index >= self.last_log_index())
  }

  /// The vector position of the one-based log `index`, or `None` when it is not in the in-memory log
  /// (it is at or before the snapshot, or beyond the end). With no snapshot (`snapshot_index == 0`) this
  /// is `index - 1` — the un-compacted layout.
  fn position(&self, index: u64) -> Option<usize> {
    if index <= self.snapshot_index {
      return None;
    }
    usize::try_from(index - self.snapshot_index - 1).ok()
  }

  /// The index of the last log entry (the snapshot index for an empty log; zero when neither exists).
  /// Raft indexes entries from one.
  pub fn last_log_index(&self) -> u64 {
    self
      .snapshot_index
      .saturating_add(u64::try_from(self.log.len()).unwrap_or(u64::MAX))
  }

  /// The term of the last log entry (the snapshot term for an empty log; zero when neither exists).
  pub fn last_log_term(&self) -> u64 {
    self
      .log
      .last()
      .map_or(self.snapshot_term, |entry| entry.term)
  }

  /// The term of the entry at the one-based `index`, or `None` if it is not individually known: index
  /// zero (the empty-log sentinel), an index below the snapshot (folded into it), or beyond the log's
  /// end. The snapshot's own index returns the snapshot term. The consistency check treats the sentinel
  /// specially.
  fn entry_term(&self, index: u64) -> Option<u64> {
    if index == 0 {
      return None;
    }
    if index == self.snapshot_index {
      return Some(self.snapshot_term);
    }
    let position = self.position(index)?;
    self.log.get(position).map(|entry| entry.term)
  }

  /// The highest index known committed classically: a majority's logs hold it at their leader's term. It is
  /// what a follower learns, what is retained, and what a window drops slots under — a fast commit does not
  /// raise it, since a fast-committed value is in no majority's logs and a successor may need its slots
  /// (`docs/wip/research/consensus-enhancements.md` §4).
  pub fn commit_index(&self) -> u64 {
    self.commit_index
  }

  /// The index through which this node knows the log committed: its commit index and, while it leads, the
  /// indices past it that fast quorums chose, in order — what the caller applies and acknowledges.
  pub fn committed_through(&self) -> u64 {
    self.commit_index.max(self.fast_through)
  }

  /// The committed log entries not yet folded into the snapshot, in order (the entries the caller applies
  /// after the snapshotted prefix), through [`committed_through`](Self::committed_through). With no snapshot
  /// this is the whole committed prefix.
  pub fn committed_entries(&self) -> &[LogEntry] {
    let committed_above_snapshot = self.committed_through().saturating_sub(self.snapshot_index);
    let count = usize::try_from(committed_above_snapshot).unwrap_or(usize::MAX);
    &self.log[..count.min(self.log.len())]
  }

  /// The index up to which the log has been compacted into a snapshot (zero when nothing is compacted).
  pub fn snapshot_index(&self) -> u64 {
    self.snapshot_index
  }

  /// How far every follower this node replicates to holds the log: while leading, the least match index
  /// among its replication targets (its own last index when it replicates to no one); otherwise its commit
  /// index, since a follower serves no one. What a leader compacts past this point, a follower still
  /// catching up can only take as the whole snapshot.
  pub fn replicated_through(&self) -> u64 {
    if self.role != Role::Leader {
      return self.commit_index;
    }
    self
      .replication_targets()
      .into_iter()
      .filter(|target| *target != self.id)
      .map(|target| self.match_of(target))
      .min()
      .unwrap_or_else(|| self.last_log_index())
  }

  /// The wire bytes of the log entries above the snapshot through `index` — what compacting to `index`
  /// would replace with a snapshot ([`LogEntry::encoded_len`]).
  pub fn log_bytes_through(&self, index: u64) -> usize {
    let through = usize::try_from(index.saturating_sub(self.snapshot_index))
      .unwrap_or(usize::MAX)
      .min(self.log.len());
    self
      .log
      .get(..through)
      .unwrap_or(&[])
      .iter()
      .map(LogEntry::encoded_len)
      .fold(0usize, usize::saturating_add)
  }

  /// Compacts the log by folding the committed prefix up to `up_to` into a snapshot and discarding those
  /// entries, so the log stays bounded (§7). Only committed entries are compacted — `up_to` must be at or
  /// below the commit index and beyond the current snapshot — and the snapshot term is recorded so the
  /// consistency check at the boundary still holds. Returns whether it compacted. The caller must have
  /// captured the state machine's state at `up_to` first (the groups do, `crate::fold`); a follower far
  /// enough behind to need a discarded entry is sent the snapshot
  /// ([`install_snapshot_for`](RaftNode::install_snapshot_for)).
  pub fn compact(&mut self, up_to: u64, state: Vec<u8>) -> bool {
    if up_to <= self.snapshot_index || up_to > self.commit_index {
      return false;
    }
    let Some(term) = self.entry_term(up_to) else {
      return false;
    };
    let discard = usize::try_from(up_to - self.snapshot_index).unwrap_or(usize::MAX);
    let discard = discard.min(self.log.len());
    // A configuration entry in the discarded prefix would take its voter set with it — fold the most
    // recent one into the base configuration so the effective configuration is preserved. (A later
    // configuration entry that survives the compaction still dominates it, being derived from the log.)
    if let Some(config) = self.log[..discard]
      .iter()
      .rev()
      .find_map(|entry| entry.config.clone())
    {
      self.voters = config.voters;
      self.joint = config.joint;
    }
    self.retention_pending = true;
    self.log.drain(0..discard);
    self.config_entries = self.config_entries.split_off(&up_to.saturating_add(1));
    self.snapshot_index = up_to;
    self.snapshot_term = term;
    self.snapshot_data = state;
    true
  }

  /// The [`InstallSnapshot`] to send `follower` when it has fallen below the leader's snapshot (the
  /// entries it needs were compacted away), else `None`. The leader calls this when
  /// [`replicate_to`](RaftNode::replicate_to) returns `None`.
  pub fn install_snapshot_for(&self, follower: HostId) -> Option<InstallSnapshot> {
    if self.role != Role::Leader || self.snapshot_index == 0 {
      return None;
    }
    let next = self.next_index.get(&follower).copied().unwrap_or(1).max(1);
    if next > self.snapshot_index {
      return None; // an append can still reach it
    }
    Some(InstallSnapshot {
      term: self.current_term,
      leader: self.id,
      last_included_index: self.snapshot_index,
      last_included_term: self.snapshot_term,
      // The configuration at the snapshot is the base — `compact` folded any discarded configuration
      // entry into it, and no surviving log entry precedes the snapshot.
      config: VoterConfig {
        voters: self.voters.clone(),
        joint: self.joint.clone(),
      },
      state: self.snapshot_data.clone(),
    })
  }

  /// Handles a received [`InstallSnapshot`] as a follower (Raft §7). A stale-term snapshot is rejected; a
  /// current-or-newer one is installed: if the follower holds an entry at the snapshot's last included
  /// index and term it keeps the following entries, otherwise it discards its whole log; then it adopts
  /// the snapshot index, term and state and advances its commit index to at least the snapshot. The
  /// caller applies the state to its state machine. Returns the reply.
  pub fn on_install_snapshot(&mut self, request: InstallSnapshot) -> InstallSnapshotReply {
    if request.term < self.current_term {
      return self.snapshot_reply(0);
    }
    self.recognize_leader(request.term, request.leader);

    if request.last_included_index > self.snapshot_index {
      self.retention_pending = true;
      let keeps_suffix =
        self.entry_term(request.last_included_index) == Some(request.last_included_term);
      if keeps_suffix {
        let discard = usize::try_from(request.last_included_index - self.snapshot_index)
          .unwrap_or(usize::MAX)
          .min(self.log.len());
        self.log.drain(0..discard);
        self.config_entries = self
          .config_entries
          .split_off(&request.last_included_index.saturating_add(1));
      } else {
        self.log.clear();
        self.config_entries.clear();
      }
      self.snapshot_index = request.last_included_index;
      self.snapshot_term = request.last_included_term;
      self.snapshot_data = request.state;
      // Adopt the configuration at the snapshot as the base, so the effective configuration is preserved
      // now that the log entries that carried it are gone.
      self.voters = request.config.voters;
      self.joint = request.config.joint;
      self.commit_index = self.commit_index.max(request.last_included_index);
      self.prune_committed();
    }
    self.snapshot_reply(self.commit_index)
  }

  /// Declines an [`InstallSnapshot`] whose state the caller could not decode: the sender is recognized as
  /// the leader of its term exactly as for an install (a newer term steps this node down, and the contact
  /// defers its election), but nothing of the snapshot is adopted, and the reply's zero match index tells the
  /// leader this follower holds nothing more than before.
  pub fn decline_snapshot(&mut self, request: &InstallSnapshot) -> InstallSnapshotReply {
    if request.term >= self.current_term {
      self.recognize_leader(request.term, request.leader);
    }
    self.snapshot_reply(0)
  }

  /// Defers to the leader of `term` (a current-or-newer term): a newer term steps this node down, and a
  /// leader exists for the term, so a candidate stands down and a pre-vote that would disrupt it is refused
  /// (§9.6).
  fn recognize_leader(&mut self, term: u64, leader: HostId) {
    if term > self.current_term {
      self.step_down(term);
    }
    self.read_round = None;
    self.role = Role::Follower;
    self.has_leader = true;
    self.leader_hint = Some(leader);
  }

  /// A snapshot reply with this node's current term and `match_index`.
  fn snapshot_reply(&self, match_index: u64) -> InstallSnapshotReply {
    InstallSnapshotReply {
      follower: self.id,
      term: self.current_term,
      match_index,
    }
  }

  /// Handles a follower's [`InstallSnapshotReply`] as the leader: a newer term steps us down; otherwise
  /// the follower's log matches through the index it reports (a declined snapshot reports zero and moves
  /// nothing), so its `match_index`/`next_index` advance to it — never backward, and never to this leader's
  /// own snapshot index, which may have moved on since the snapshot left — and the commit index may
  /// advance.
  pub fn on_install_snapshot_reply(&mut self, reply: InstallSnapshotReply) {
    if reply.term > self.current_term {
      self.step_down(reply.term);
      return;
    }
    if self.role != Role::Leader || reply.term != self.current_term {
      return;
    }
    self.contacts.insert(reply.follower);
    if reply.match_index > 0 {
      self.record_match(reply.follower, reply.match_index);
      self.advance_leader_commit();
    }
  }

  /// Records that `follower`'s log matches this leader's through `index`: its match index only grows within
  /// a term (the leader's log is append-only while it leads, so a matched prefix stays matched), and its
  /// next index is at least the entry after it. A late reply to an earlier request therefore never moves
  /// either back.
  fn record_match(&mut self, follower: HostId, index: u64) {
    let matched = self.match_of(follower).max(index);
    self.match_index.insert(follower, matched);
    self.probing.remove(&follower);
    let next = self.next_index.get(&follower).copied().unwrap_or(1);
    self
      .next_index
      .insert(follower, next.max(matched.saturating_add(1)));
    // A staged member that has taken everything its round replicates completes the round: within one
    // window it is caught up; otherwise the next round replicates what the leader holds now (thesis
    // §4.2.1's rounds, which shrink as it gains).
    let last = self.last_log_index();
    if let Some(staging) = self.staging.get_mut(&follower)
      && !staging.caught_up
      && matched >= staging.round_end
    {
      if staging.window_passed {
        staging.round_end = last;
        staging.window_passed = false;
      } else {
        staging.caught_up = true;
      }
    }
  }

  /// Appends `command` to the leader's own log at the current term and updates its self-match, so a
  /// single-voter leader commits it at once (Raft §5.3, leader append). A non-leader ignores the append
  /// and reports `false` — only the leader proposes.
  pub fn append_command(&mut self, command: Vec<u8>) -> bool {
    // A leader handing off leadership stops accepting proposals (thesis §3.10), so the target's log can
    // catch up to a fixed end and the election it starts is won by a log that holds every entry.
    // A leader whose fast track is open proposes through it ([`propose_fast`](Self::propose_fast)): a
    // classic entry at an index open to fast votes could contradict a value a fast quorum chose there.
    if self.role != Role::Leader || self.active_transfer().is_some() || self.open_from > 0 {
      return false;
    }
    self.leader_append(LogEntry::command(self.current_term, command));
    true
  }

  /// Appends `entry` to this leader's log, publishes it, and advances the commit index. The first entry a
  /// leader appends after its recovery is its **sync point**: a follower whose log holds it is synced to this
  /// term ([`AppendEntries::sync_index`]).
  fn leader_append(&mut self, entry: LogEntry) {
    self.retention_pending = true;
    self.push_entry(entry);
    if self.sync_index == 0 {
      self.sync_index = self.last_log_index();
    }
    self.advance_leader_commit();
  }

  /// Sets the wire bytes this node's window may hold — the caller's append budget
  /// (`raft_wire::append_batch_bytes`), since a window holds at most what one append carries. Zero, the
  /// default, holds none: no entry is buffered and no fast vote is cast.
  pub fn set_window_budget(&mut self, bytes: usize) {
    self.window_budget = bytes;
  }

  /// What the window's paths did over this node's life.
  pub fn window_counters(&self) -> WindowCounters {
    self.window_counters
  }

  /// The window slots this node holds, by index.
  pub fn window(&self) -> Vec<SlotReport> {
    self.window_above(0)
  }

  /// Opens this term's **fast track** (`docs/wip/research/consensus-enhancements.md` §3.7): from the next
  /// index on, commands are proposed straight to every voter ([`propose_fast`](Self::propose_fast)), and this
  /// leader decides each index from the votes. Only a leader that has appended its sync point opens it, with
  /// a window budget, on a committed configuration that is not joint and with no transfer in flight; once
  /// open, it stays open for the term, and a classic append or a membership change is refused. Returns whether
  /// it opened.
  ///
  /// The configuration must have committed because the votes are counted against it, by this leader and by
  /// any successor's recovery: a committed configuration is in every later leader's log, and a later change
  /// begins only in a classic term whose leader recovered these votes first. With `C_new` appended and not
  /// committed, a successor lacking it would count the joint configuration and treat a chosen index as free.
  pub fn open_fast_track(&mut self) -> bool {
    if self.role != Role::Leader
      || self.sync_index == 0
      || self.window_budget == 0
      || self.open_from > 0
      || self.in_joint_configuration()
      || self.latest_config_index() > self.commit_index
      || self.active_transfer().is_some()
    {
      return false;
    }
    self.open_from = self.last_log_index().saturating_add(1);
    self.open_announced = (self.current_term, self.open_from);
    true
  }

  /// The first index open to the fast track this term as this node knows it announced, or zero when closed or
  /// announced in another term.
  fn fast_open_from(&self) -> u64 {
    let (term, open_from) = self.open_announced;
    if term == self.current_term {
      open_from
    } else {
      0
    }
  }

  /// Whether this node may propose on the fast track now: the track is open this term, as it knows, and it is
  /// synced to the term's leader.
  pub fn fast_track_open(&self) -> bool {
    self.fast_open_from() > 0 && self.synced_term == self.current_term
  }

  /// Proposes `command` on the fast track at the lowest index this node knows unused — past the opening, its
  /// log, its window and its own last proposal — returning the proposal to send every voter (itself
  /// included), or `None` when the track is not open to it. The caller learns the outcome from the log:
  /// another command decided at the index means this one was not chosen, and it proposes again.
  pub fn propose_fast(&mut self, command: Vec<u8>) -> Option<FastPropose> {
    // A leader handing off accepts no proposals (thesis §3.10), on either track.
    if !self.fast_track_open() || self.active_transfer().is_some() {
      return None;
    }
    let used = self
      .window
      .keys()
      .next_back()
      .copied()
      .unwrap_or(0)
      .max(self.last_log_index())
      .max(self.fast_proposed);
    let index = self.fast_open_from().max(used.saturating_add(1));
    self.fast_proposed = index;
    Some(FastPropose {
      term: self.current_term,
      proposer: self.id,
      index,
      command,
    })
  }

  /// The index this leader's fast track is stalled at, if any (§3.7): its next undecided index, while votes wait
  /// at or above it and too few voters have voted there for a decision. The caller fills it
  /// ([`fill_hole`](Self::fill_hole)) once it has stalled as long as a lost vote takes to be sent again.
  pub fn stalled_index(&self) -> Option<u64> {
    if self.role != Role::Leader || self.open_from == 0 || self.active_transfer().is_some() {
      return None;
    }
    let next = self.last_log_index().saturating_add(1);
    let waiting = self
      .fast_votes
      .keys()
      .next_back()
      .is_some_and(|index| *index >= next);
    let heard: BTreeSet<HostId> = self
      .fast_votes
      .get(&next)
      .map(|votes| votes.keys().copied().collect())
      .unwrap_or_default();
    (waiting && !self.is_majority(&heard)).then_some(next)
  }

  /// The no-op this leader proposes at its stalled index `index` on its own fast track, to send every voter —
  /// Fast Paxos's coordinator-run round for an instance its fast round left undecided (Lamport, *Fast Paxos*,
  /// 2006, §3.3): a voter that voted there sends its vote again, and one that did not votes the no-op, so the
  /// leader hears a classic quorum and decides by the ballot rule. The ballot rule does not look at what is
  /// proposed, so this is safe as any proposal is. Its own vote is cast like any voter's, when the caller hands
  /// it the proposal. `None` unless `index` is stalled.
  pub fn fill_hole(&mut self, index: u64) -> Option<FastPropose> {
    (self.stalled_index() == Some(index)).then(|| FastPropose {
      term: self.current_term,
      proposer: self.id,
      index,
      command: Vec::new(),
    })
  }

  /// Handles a fast proposal as a voter: a voter synced to this term's leader, at an index the leader opened,
  /// above its log and within its window's span and budget, votes the command — unless it already voted there
  /// this term (its vote stands, and is sent again) or holds this term's decision there. Returns the vote to
  /// send the term's leader.
  pub fn on_fast_propose(&mut self, proposal: FastPropose) -> Option<FastVote> {
    let open_from = self.fast_open_from();
    let index = proposal.index;
    let floor = self.last_log_index();
    let reach = floor.saturating_add(self.window_span());
    if proposal.term != self.current_term
      || !self.is_voter(self.id)
      || self.synced_term != self.current_term
      || open_from == 0
      || index < open_from
      || index <= floor
      || index > reach
    {
      return None;
    }
    let vote = |command: Vec<u8>| FastVote {
      term: proposal.term,
      voter: self.id,
      index,
      command,
    };
    if let Some(held) = self.window.get(&index) {
      if held.fast && held.term == self.current_term {
        return Some(vote(held.entry.command.clone()));
      }
      if held.ballot() >= (self.current_term, false) {
        return None;
      }
    }
    let entry = LogEntry::command(self.current_term, proposal.command);
    let replaced = self
      .window
      .get(&index)
      .map_or(0, |held| held.entry.encoded_len());
    let held: usize = self
      .window
      .values()
      .map(|slot| slot.entry.encoded_len())
      .fold(0, usize::saturating_add);
    if held
      .saturating_sub(replaced)
      .saturating_add(entry.encoded_len())
      > self.window_budget
    {
      return None;
    }
    let command = entry.command.clone();
    self.window.insert(
      index,
      WindowSlot {
        term: self.current_term,
        fast: true,
        entry,
      },
    );
    self.retention_pending = true;
    Some(vote(command))
  }

  /// Handles a fast vote as the leader: tallies it (one vote per voter per index, above the commit index
  /// and within the window's span above the log), decides the next indices in order while a classic quorum
  /// has voted at each ([`decide_from_votes`](Self::decide_from_votes)), and commits each decided index a
  /// fast quorum voted — a vote arriving after the decision still counts toward the fast quorum, as the
  /// round's vote it is.
  pub fn on_fast_vote(&mut self, vote: FastVote) {
    if self.role != Role::Leader
      || vote.term != self.current_term
      || self.open_from == 0
      || vote.index < self.open_from
      || vote.index <= self.committed_through()
      || vote.index > self.last_log_index().saturating_add(self.window_span())
      || !self.is_voter(vote.voter)
    {
      return;
    }
    // A vote of this term shows the voter follows this leader, as a reply does.
    self.contacts.insert(vote.voter);
    self
      .fast_votes
      .entry(vote.index)
      .or_default()
      .insert(vote.voter, vote.command);
    self.decide_from_votes();
    self.count_fast_quorums();
  }

  /// Marks as chosen each decided index above the commit index whose command a fast quorum of this term's
  /// votes carries, and advances the commit index over them.
  fn count_fast_quorums(&mut self) {
    let fast = fast_quorum(self.all_voters().len());
    let (committed, last) = (self.committed_through(), self.last_log_index());
    let mut chosen = Vec::new();
    // Filtered rather than ranged: `BTreeMap::range` panics on a start past its end, as a fully committed
    // log's `committed + 1..=last` is.
    for (index, votes) in self
      .fast_votes
      .iter()
      .filter(|(index, _)| **index > committed && **index <= last)
    {
      let Some(entry) = self.position(*index).and_then(|at| self.log.get(at)) else {
        continue;
      };
      let carried = votes
        .values()
        .filter(|command| **command == entry.command)
        .count();
      if entry.term == self.current_term && carried >= fast && !self.fast_chosen.contains(index) {
        chosen.push(*index);
      }
    }
    for index in chosen {
      self.fast_chosen.insert(index);
      self.window_counters.fast_commits = self.window_counters.fast_commits.saturating_add(1);
    }
    self.advance_leader_commit();
    let committed = self.committed_through();
    self.fast_votes.retain(|index, _| *index > committed);
  }

  /// Decides this leader's next index from its votes, and the ones after while each has them: once a classic
  /// quorum of voters has voted there, the command a fast quorum could have chosen — at least
  /// `heard + |F| − n` of the heard votes (Fast Paxos's rule; the most voted, necessarily) — or else the most
  /// voted, is appended at this term. It commits when a fast quorum voted it
  /// ([`count_fast_quorums`](Self::count_fast_quorums)), or else by a majority's logs as any entry of the term.
  fn decide_from_votes(&mut self) {
    // A leader handing off decides nothing (thesis §3.10: it stops accepting proposals, so the target's log can
    // match its own); the votes wait in the voters' windows for the successor's recovery.
    if self.active_transfer().is_some() {
      return;
    }
    loop {
      let next = self.last_log_index().saturating_add(1);
      let Some(votes) = self.fast_votes.get(&next) else {
        return;
      };
      let heard: BTreeSet<HostId> = votes.keys().copied().collect();
      if !self.is_majority(&heard) {
        return;
      }
      let mut counts: BTreeMap<&Vec<u8>, usize> = BTreeMap::new();
      for command in votes.values() {
        let count = counts.entry(command).or_default();
        *count = count.saturating_add(1);
      }
      // The most voted command, ties to the least bytes so every run decides alike.
      let Some(command) = counts
        .iter()
        .max_by(|left, right| left.1.cmp(right.1).then(right.0.cmp(left.0)))
        .map(|(command, _)| (*command).clone())
      else {
        return;
      };
      self.leader_append(LogEntry::command(self.current_term, command));
      self.window_counters.decided_from_votes =
        self.window_counters.decided_from_votes.saturating_add(1);
    }
  }

  /// Builds the [`AppendEntries`] to send `follower` and records what went: a batch of entries — as many as
  /// fit in `budget` wire bytes ([`LogEntry::encoded_len`]), and always at least one when any is owed, so an
  /// entry larger than the budget still goes, alone — with the previous position for the consistency check.
  /// Empty entries make it a heartbeat. Where the batch starts (`docs/wip/research/consensus-enhancements.md`
  /// §3.5; thesis §10.2.2):
  /// - a follower whose place is a guess (probing) is sent from its `next_index`, one batch until one is
  ///   acknowledged;
  /// - a confirmed follower is pipelined: sent the next batch before the last is acknowledged, while what is
  ///   in flight beyond its first unacknowledged batch fits its window — the group's one window budget, what
  ///   it can hold ahead of a hole ([`room_ahead`](Self::room_ahead)) — and `next_index` moves past what went;
  /// - otherwise it is sent its first unacknowledged batch again, which is a heartbeat when it holds
  ///   everything.
  ///
  /// With no window nothing goes ahead, so every send starts at the first unacknowledged entry. Returns `None`
  /// if this node is not the leader, or when the entries the follower needs were compacted away (it needs
  /// [`install_snapshot_for`](RaftNode::install_snapshot_for), and is set to probe from there).
  pub fn replicate_to(&mut self, follower: HostId, budget: usize) -> Option<AppendEntries> {
    if self.role != Role::Leader {
      return None;
    }
    let next = self.next_index.get(&follower).copied().unwrap_or(1).max(1);
    let first_unacknowledged = self.match_of(follower).saturating_add(1);
    let probing = self.probing.contains(&follower);
    let resend = !probing
      && next > first_unacknowledged
      && !self.room_ahead(first_unacknowledged, next, budget);
    let from = if resend { first_unacknowledged } else { next };
    let prev_log_index = from.saturating_sub(1);
    // The entry before the batch has been compacted away — the follower needs an install-snapshot
    // ([`install_snapshot_for`](RaftNode::install_snapshot_for)), not an append; it probes from there.
    if prev_log_index < self.snapshot_index {
      self.next_index.insert(follower, from);
      self.probing.insert(follower);
      return None;
    }
    let prev_log_term = if prev_log_index == 0 {
      0
    } else {
      self.entry_term(prev_log_index).unwrap_or(0)
    };
    let entries = self.batch_from(from, budget).to_vec();
    if !probing && !resend {
      let sent = u64::try_from(entries.len()).unwrap_or(u64::MAX);
      self.next_index.insert(follower, from.saturating_add(sent));
      if from > first_unacknowledged {
        self.window_counters.sent_ahead = self.window_counters.sent_ahead.saturating_add(1);
      }
    }
    let mut priorities: Vec<(HostId, ElectionPriority)> = self
      .all_voters()
      .into_iter()
      .filter(|voter| *voter != self.id)
      .filter_map(|voter| {
        self
          .priorities
          .get(&voter)
          .map(|priority| (voter, *priority))
      })
      .collect();
    priorities.push((self.id, self.own_priority));
    Some(AppendEntries {
      read_context: self.read_round.as_ref().map_or(0, |read| read.context),
      term: self.current_term,
      leader: self.id,
      prev_log_index,
      prev_log_term,
      entries,
      leader_commit: self.commit_index,
      priorities,
      sync_index: self.sync_index,
      open_from: self.open_from,
    })
  }

  /// The entries a batch from `from` carries within `budget` wire bytes, and always the first when any is owed.
  fn batch_from(&self, from: u64, budget: usize) -> &[LogEntry] {
    let start = usize::try_from(from.saturating_sub(self.snapshot_index).saturating_sub(1))
      .unwrap_or(usize::MAX);
    let owed = self.log.get(start..).unwrap_or(&[]);
    let mut spent = 0usize;
    let batch = owed
      .iter()
      .take_while(|entry| {
        let first = spent == 0;
        spent = spent.saturating_add(entry.encoded_len());
        first || spent <= budget
      })
      .count();
    owed.get(..batch).unwrap_or(&[])
  }

  /// Whether a follower whose first unacknowledged entry is `first_unacknowledged` may be sent the batch from
  /// `next` now, ahead of its acknowledgements:
  /// - a resend would not reach the log's end — the backlog is more than one batch; when a resend would carry
  ///   the whole backlog it is the better send, since it also recovers a lost batch within one send, where a
  ///   batch sent ahead finds the loss only through the follower's refusal, a round trip later (a first cut
  ///   resent whenever a resend reached past `next`, and at a steady rate just short of a batch a period never
  ///   sent ahead however far the backlog grew);
  /// - there is a batch at `next`;
  /// - it and what is in flight beyond the first unacknowledged batch fit the window, the group's one budget:
  ///   what the follower can hold ahead of a hole, so nothing sent ahead is lost to one (§3.5).
  ///
  /// A follower whose first unacknowledged entry was compacted away needs a snapshot, not more entries.
  fn room_ahead(&self, first_unacknowledged: u64, next: u64, budget: usize) -> bool {
    let bytes = |entries: &[LogEntry]| {
      entries
        .iter()
        .map(LogEntry::encoded_len)
        .fold(0, usize::saturating_add)
    };
    if first_unacknowledged <= self.snapshot_index {
      return false;
    }
    let resent = self.batch_from(first_unacknowledged, budget);
    let resend_reaches =
      first_unacknowledged.saturating_add(u64::try_from(resent.len()).unwrap_or(u64::MAX));
    if resend_reaches > self.last_log_index() {
      return false;
    }
    let coming = bytes(self.batch_from(next, budget));
    let first = bytes(resent);
    let sent = next.saturating_sub(first_unacknowledged);
    let start = usize::try_from(
      first_unacknowledged
        .saturating_sub(self.snapshot_index)
        .saturating_sub(1),
    )
    .unwrap_or(usize::MAX);
    let in_flight = bytes(
      self
        .log
        .get(start..)
        .unwrap_or(&[])
        .get(..usize::try_from(sent).unwrap_or(usize::MAX))
        .unwrap_or(&[]),
    );
    coming > 0 && in_flight.saturating_sub(first).saturating_add(coming) <= self.window_budget
  }

  /// Handles a received [`AppendEntries`] as a follower (Raft §5.3). A stale-term append is rejected. A
  /// current-or-newer term is recognised (this node becomes a follower for it). The append succeeds only
  /// when the log matches at `prev_log_index`/`prev_log_term`; then any conflicting suffix is truncated,
  /// the new entries appended, and the commit index advanced toward the leader's. Returns the reply,
  /// carrying on success the last index now matched.
  pub fn on_append_entries(&mut self, mut request: AppendEntries) -> AppendReply {
    if request.term < self.current_term {
      return self.append_reply(false, 0, 0);
    }
    // A current-term append means a leader exists for our term — defer to it (a candidate steps down)
    // and note the contact, so we refuse pre-votes that would disrupt this leader (§9.6).
    self.recognize_leader(request.term, request.leader);
    // Within a term the opening only goes from closed to one fixed index, so a reordered append that predates
    // it does not close the track again.
    let (term, known) = self.open_announced;
    let known = if term == request.term { known } else { 0 };
    self.open_announced = (request.term, known.max(request.open_from));
    // The leader's priority table is the one every follower ranks itself against (§3.4).
    self.priorities = std::mem::take(&mut request.priorities)
      .into_iter()
      .collect();

    // An append anchored inside our committed prefix — a late or duplicated copy, or the first append to a
    // member that joined with the committed prefix, sent before the leader learned how far it is — agrees
    // with us through our commit index, since every committed entry is the same on every server. So its
    // entries at or below our commit index are skipped and the rest are taken as if it were anchored there;
    // one that ends inside our committed prefix brings nothing new, and the reply says we match through our
    // commit index. Without this, a previous index below our snapshot failed the check, each refusal backed
    // the leader up one entry, and the append that finally anchored at zero landed compacted entries on the
    // end of our log. (etcd's `handleAppendEntries` answers such an append with its commit index and drops
    // it; taking the new entries saves the round trip a joining member would otherwise spend.)
    if request.prev_log_index < self.commit_index {
      let covered =
        usize::try_from(self.commit_index - request.prev_log_index).unwrap_or(usize::MAX);
      if covered >= request.entries.len() {
        return self.append_reply(true, self.commit_index, request.read_context);
      }
      request.entries.drain(..covered);
      request.prev_log_index = self.commit_index;
      request.prev_log_term = self.entry_term(self.commit_index).unwrap_or(0);
    }

    // Consistency check: our log must contain the previous entry with the leader's term. A follower synced
    // to this leader keeps the leader's own-term entries that arrived ahead of a hole in its window, so they
    // join its log the moment the hole fills (§3.5's out-of-order acknowledgement).
    if request.prev_log_index > 0
      && self.entry_term(request.prev_log_index) != Some(request.prev_log_term)
    {
      self.buffer(&request);
      return self.conflict_reply(request.prev_log_index, request.read_context);
    }

    // Append, truncating the first conflicting entry and everything after it.
    let mut index = request.prev_log_index;
    for entry in request.entries {
      index = index.saturating_add(1);
      match self.entry_term(index) {
        Some(term) if term == entry.term => {} // already present and matching — keep it
        Some(_) => {
          self.truncate_from(index);
          self.push_entry(entry);
        }
        None => {
          self.retention_pending = true;
          self.push_entry(entry);
        }
      }
    }

    // Advance the commit index to the leader's, but no further than the entries we now hold.
    if request.leader_commit > self.commit_index {
      let committed = request.leader_commit.min(index);
      self.retention_pending |= self.commit_index != committed;
      self.commit_index = committed;
    }
    self.observe_sync(request.term, request.sync_index);
    let index = self.absorb(request.term).max(index);
    self.prune_committed();
    self.append_reply(true, index, request.read_context)
  }

  /// Keeps the leader's entries of its own term from a refused append in the window, when this node is synced
  /// to that leader: each above the log's end, within [`window_span`](Self::window_span) and the window's
  /// budget, where no slot of its ballot or a higher one is held.
  fn buffer(&mut self, request: &AppendEntries) {
    if self.synced_term != request.term || request.term != self.current_term {
      return;
    }
    let (floor, reach) = (
      self.last_log_index(),
      self.last_log_index().saturating_add(self.window_span()),
    );
    let mut index = request.prev_log_index;
    for entry in &request.entries {
      index = index.saturating_add(1);
      if entry.term != request.term || index <= floor || index > reach {
        continue;
      }
      let slot = WindowSlot {
        term: request.term,
        fast: false,
        entry: entry.clone(),
      };
      if self.window_holds_at_least(index, slot.ballot()) || !self.window_fits(&slot.entry) {
        continue;
      }
      self.window.insert(index, slot);
      self.retention_pending = true;
      self.window_counters.buffered = self.window_counters.buffered.saturating_add(1);
    }
  }

  /// Whether the window holds a slot at `index` of `ballot` or a higher one.
  fn window_holds_at_least(&self, index: u64, ballot: (u64, bool)) -> bool {
    self
      .window
      .get(&index)
      .is_some_and(|held| held.ballot() >= ballot)
  }

  /// Whether `entry` fits the window's budget beside what it holds.
  fn window_fits(&self, entry: &LogEntry) -> bool {
    let held: usize = self
      .window
      .values()
      .map(|slot| slot.entry.encoded_len())
      .fold(0, usize::saturating_add);
    held.saturating_add(entry.encoded_len()) <= self.window_budget
  }

  /// A successful append from the leader of `term` whose sync point is `sync_index`: once this node's log
  /// holds that leader's entry there, it is synced to `term`. Its slots of older terms stay: dropping them here
  /// lost a chosen value in the prefix model's 18-step history (a later leader's truncation erased the synced
  /// log entries that were then the only record); they go under a classic commit, as every slot does.
  fn observe_sync(&mut self, term: u64, sync_index: u64) {
    if sync_index == 0 || self.synced_term >= term || self.entry_term(sync_index) != Some(term) {
      return;
    }
    self.synced_term = term;
    self.retention_pending = true;
  }

  /// Moves into the log the leader's buffered entries of `term` that now continue it, returning the index of
  /// the log's last entry after.
  fn absorb(&mut self, term: u64) -> u64 {
    loop {
      let next = self.last_log_index().saturating_add(1);
      let Some(slot) = self
        .window
        .get(&next)
        .filter(|slot| !slot.fast && slot.term == term && slot.entry.term == term)
      else {
        return self.last_log_index();
      };
      let entry = slot.entry.clone();
      self.push_entry(entry);
      self.retention_pending = true;
      self.window_counters.absorbed = self.window_counters.absorbed.saturating_add(1);
    }
  }

  /// Drops window slots at classically committed indices: Raft's election rule puts a classically committed
  /// entry, and every value below it, in every later leader's log, so a slot there records nothing a recovery
  /// could need. A fast commit is not enough: pruning under it lost a chosen value in the prefix model (a
  /// successor that lacked it outranked the log that held it, and found the slots gone).
  fn prune_committed(&mut self) {
    let committed = self.commit_index;
    let before = self.window.len();
    self.window.retain(|index, _| *index > committed);
    let pruned = u64::try_from(before.saturating_sub(self.window.len())).unwrap_or(u64::MAX);
    self.window_counters.pruned = self.window_counters.pruned.saturating_add(pruned);
    self.retention_pending |= pruned > 0;
  }

  /// Handles a follower's [`AppendReply`] as the leader (Raft §5.3). A newer term steps us down. On
  /// success the follower's `match_index`/`next_index` advance and the commit index may advance; on the
  /// consistency-check failure `next_index` backs up one so the next append tries an earlier position.
  pub fn on_append_reply(&mut self, reply: AppendReply) {
    if reply.term > self.current_term {
      self.step_down(reply.term);
      return;
    }
    if self.role != Role::Leader || reply.term != self.current_term {
      return;
    }
    if self.all_voters().contains(&reply.follower)
      && let Some(read) = self.read_round.as_mut()
      && reply.read_context == read.context
      && reply.term == read.term
    {
      read.confirmed.insert(reply.follower);
    }
    // Any same-term reply proves the follower is reachable this CheckQuorum window, and carries its priority.
    self.contacts.insert(reply.follower);
    self.priorities.insert(reply.follower, reply.priority);
    if reply.success {
      self.record_match(reply.follower, reply.match_index);
      self.advance_leader_commit();
    } else {
      self.back_up(reply);
    }
  }

  /// Moves `next_index` back after a consistency-check refusal, by the follower's conflict hint (Raft
  /// §5.3): past this leader's last entry of the conflicting term when it holds that term, else to the
  /// follower's hint. The result is strictly below the current next index — the hint is at or below the
  /// refused previous index, and a leader holding the follower's conflicting term holds it only before
  /// that index (terms never decrease along a log, and log matching rules out a later run) — and never at
  /// or below the follower's match index, so a late refusal of an earlier request cannot undo progress.
  fn back_up(&mut self, reply: AppendReply) {
    let Some(&next) = self.next_index.get(&reply.follower) else {
      return;
    };
    let hint = if reply.conflict_term == 0 {
      reply.conflict_index
    } else {
      self
        .last_index_of_term(reply.conflict_term)
        .map_or(reply.conflict_index, |last| last.saturating_add(1))
    };
    let floor = self.match_of(reply.follower).saturating_add(1);
    let backed = hint.min(next.saturating_sub(1)).max(floor).max(1);
    self.next_index.insert(reply.follower, backed);
    self.probing.insert(reply.follower);
  }

  /// The index of this leader's last entry of `term` above its snapshot, if it holds one.
  fn last_index_of_term(&self, term: u64) -> Option<u64> {
    let position = self.log.iter().rposition(|entry| entry.term == term)?;
    let position = u64::try_from(position).ok()?;
    Some(
      self
        .snapshot_index
        .saturating_add(position)
        .saturating_add(1),
    )
  }

  /// The leader's CheckQuorum tick (Raft §6.2): if the leader has not been in contact with a majority of
  /// voters since the previous check, it steps down to a follower — so a leader cut off from the cluster
  /// stops acting as leader (it will not keep serving reads or block a fresh election on the majority
  /// side). Then the contact window resets. A non-leader is unaffected, and a lone voter is always its
  /// own majority, so it never steps down (the `f = 0` degenerate).
  pub fn check_quorum(&mut self) {
    if self.role != Role::Leader {
      return;
    }
    self.age_transfer();
    self.age_staging();
    self.windows_led = self.windows_led.saturating_add(1);
    let targets: BTreeSet<HostId> = self.replication_targets().into_iter().collect();
    self.priorities.retain(|voter, _| targets.contains(voter));
    let mut reachable = self.contacts.clone();
    reachable.insert(self.id);
    if !self.is_majority(&reachable) {
      self.read_round = None;
      self.role = Role::Follower;
      self.has_leader = false;
      self.leader_hint = None;
      self.transfer = None;
      self.staging.clear();
    }
    self.contacts.clear();
  }

  /// One CheckQuorum tick of every staging not yet caught up: the round in progress has now spanned a
  /// window, and a member whose lag has not shrunk for [`STALLED_WINDOWS`] whole windows is aborted
  /// (counted, and reported by the next [`catch_up`](RaftNode::catch_up)).
  fn age_staging(&mut self) {
    let last = self.last_log_index();
    let matches: Vec<(HostId, u64)> = self
      .staging
      .keys()
      .map(|member| (*member, self.match_of(*member)))
      .collect();
    let mut aborted = Vec::new();
    for (member, matched) in matches {
      let Some(staging) = self.staging.get_mut(&member) else {
        continue;
      };
      if staging.caught_up {
        continue;
      }
      staging.window_passed = true;
      let lag = last.saturating_sub(matched);
      staging.strikes = if lag < staging.lag_at_tick {
        0
      } else {
        staging.strikes.saturating_add(1)
      };
      staging.lag_at_tick = lag;
      if staging.strikes >= STALLED_WINDOWS {
        aborted.push(member);
      }
    }
    for member in aborted {
      self.staging.remove(&member);
      self.forget_progress(member);
      self.stagings_aborted = self.stagings_aborted.saturating_add(1);
      self.staging_aborted = Some(member);
    }
  }

  /// Forgets a non-voter's replication progress, so the maps hold only the members this leader serves.
  fn forget_progress(&mut self, member: HostId) {
    if !self.is_voter(member) {
      self.next_index.remove(&member);
      self.match_index.remove(&member);
      self.probing.remove(&member);
    }
  }

  /// Catches up, before they vote, the members `target` adds to the voter set in force (thesis §4.2.1): each
  /// is staged — replicated to in rounds, counted toward nothing — until a round completes within one
  /// CheckQuorum window. Returns [`CatchUp::Ready`] once every added member is caught up (or `target` adds
  /// none), so the caller may begin the membership change; [`CatchUp::Pending`] while they are being caught
  /// up; [`CatchUp::Aborted`] once, for a member whose staging ended (the next call stages it afresh). A
  /// staged member `target` no longer names is unstaged. A new member's replication starts at this leader's
  /// end, as an elected leader's followers' do, so a member already holding most of the log is not sent the
  /// whole of it.
  pub fn catch_up(&mut self, target: &[HostId]) -> CatchUp {
    if self.role != Role::Leader {
      return CatchUp::NotLeader;
    }
    let voters = self.all_voters();
    let adds: BTreeSet<HostId> = target
      .iter()
      .copied()
      .filter(|member| *member != self.id && !voters.contains(member))
      .collect();
    let unstaged: Vec<HostId> = self
      .staging
      .keys()
      .copied()
      .filter(|member| !adds.contains(member))
      .collect();
    for member in unstaged {
      self.staging.remove(&member);
      self.forget_progress(member);
    }
    if let Some(member) = self.staging_aborted.take()
      && adds.contains(&member)
    {
      return CatchUp::Aborted { member };
    }
    let last = self.last_log_index();
    for member in &adds {
      if !self.staging.contains_key(member) {
        self.staging.insert(
          *member,
          Staging {
            round_end: last,
            window_passed: false,
            lag_at_tick: u64::MAX,
            strikes: 0,
            caught_up: false,
          },
        );
        self.next_index.insert(*member, last.saturating_add(1));
        self.match_index.insert(*member, 0);
        self.probing.insert(*member);
      }
    }
    if adds.iter().all(|member| {
      self
        .staging
        .get(member)
        .is_some_and(|staging| staging.caught_up)
    }) {
      CatchUp::Ready
    } else {
      CatchUp::Pending
    }
  }

  /// The members being caught up while leading, with whether each is caught up.
  pub fn staged(&self) -> Vec<(HostId, bool)> {
    if self.role != Role::Leader {
      return Vec::new();
    }
    self
      .staging
      .iter()
      .map(|(member, staging)| (*member, staging.caught_up))
      .collect()
  }

  /// The stagings this node aborted (thesis §4.2.1's abort), over its life.
  pub fn stagings_aborted(&self) -> u64 {
    self.stagings_aborted
  }

  /// Records this node's own election priority, as its caller measured it this period (§3.4).
  pub fn set_priority(&mut self, priority: ElectionPriority) {
    self.own_priority = priority;
  }

  /// Every voter's priority as this node holds it: while leading, as each follower last replied it; while
  /// following, the leader's table.
  pub fn priorities(&self) -> Vec<(HostId, ElectionPriority)> {
    self
      .priorities
      .iter()
      .map(|(voter, priority)| (*voter, *priority))
      .collect()
  }

  /// This node's election rank (§3.4): how many voters of the configuration in force that the caller holds
  /// `alive` outrank it in the table this node holds ([`ElectionPriority::outranks`]). Zero while this node's own
  /// priority is unknown or nothing outranks it — the case a group without measurements is always in.
  pub fn election_rank(&self, alive: &[HostId]) -> usize {
    self
      .all_voters()
      .into_iter()
      .filter(|voter| *voter != self.id && alive.contains(voter))
      .filter(|voter| {
        self
          .priorities
          .get(voter)
          .is_some_and(|priority| priority.outranks(&self.own_priority))
      })
      .count()
  }

  /// Hands leadership to the voter that outranks this leader most (§3.4 with thesis §3.10), when one does:
  /// a voter the caller holds `alive` whose priority outranks this node's — the least quorum round trip
  /// among them, then the lowest id. The transfer itself brings the target's log up to date before it
  /// invites it (thesis §3.10's first step), so the target need not hold every entry now: a remote voter
  /// is always a proposal cadence behind a busy leader, and a first cut that required it never handed off
  /// while proposals flowed (measured 2026-09-28: six handoffs, all before the stream began). Only after
  /// [`PRIORITY_WINDOWS`] ticks of leading, with no transfer in flight, and not after a priority transfer
  /// has aborted during this leadership — the abort bounds a failing one, and the latch keeps it from
  /// costing proposals again. Returns the target when a transfer started.
  pub fn priority_transfer(&mut self, alive: &[HostId]) -> Option<HostId> {
    if self.role != Role::Leader
      || self.windows_led < PRIORITY_WINDOWS
      || self.priority_transfer_failed
      || self.active_transfer().is_some()
    {
      return None;
    }
    let (target, _) = self
      .all_voters()
      .into_iter()
      .filter(|voter| *voter != self.id && alive.contains(voter))
      .filter_map(|voter| {
        self
          .priorities
          .get(&voter)
          .map(|priority| (voter, *priority))
      })
      .filter(|(_, priority)| priority.outranks(&self.own_priority))
      .min_by_key(|(voter, priority)| (priority.quorum_ns, voter.0))?;
    self.transfer_leadership(target).ok()?;
    self.priority_transfer_pending = true;
    self.priority_transfers = self.priority_transfers.saturating_add(1);
    Some(target)
  }

  /// The priority transfers this node started, over its life.
  pub fn priority_transfers(&self) -> u64 {
    self.priority_transfers
  }

  /// One CheckQuorum tick of the transfer in flight: past [`TRANSFER_QUORUM_CHECKS`] ticks it is aborted
  /// (counted) and the leader accepts proposals again (thesis §3.10).
  fn age_transfer(&mut self) {
    let Some(transfer) = self.active_transfer() else {
      self.transfer = None;
      return;
    };
    let quorum_checks = transfer.quorum_checks.saturating_add(1);
    if quorum_checks >= TRANSFER_QUORUM_CHECKS {
      self.transfer = None;
      self.transfers_aborted = self.transfers_aborted.saturating_add(1);
      if self.priority_transfer_pending {
        self.priority_transfer_pending = false;
        self.priority_transfer_failed = true;
      }
      // The fast track's votes that waited out the transfer are decided now, not at the next vote to arrive.
      self.decide_from_votes();
      self.count_fast_quorums();
    } else {
      self.transfer = Some(Transfer {
        quorum_checks,
        ..transfer
      });
    }
  }

  /// Starts a leadership transfer to `target` (thesis §3.10): from now until the transfer ends the leader
  /// refuses new proposals, keeps replicating, and — once `target`'s log matches its own —
  /// [`take_timeout_now`](RaftNode::take_timeout_now) yields the invitation that makes `target` campaign at
  /// once. The transfer ends when this node stops leading (the target won, or anything else deposed it) or
  /// is aborted at the second CheckQuorum tick. Refused, typed, when this node is not the leader, `target`
  /// is itself or not a voter, or a transfer is already in flight.
  pub fn transfer_leadership(&mut self, target: HostId) -> Result<(), TransferRefusal> {
    if self.role != Role::Leader {
      return Err(TransferRefusal::NotLeader);
    }
    if target == self.id {
      return Err(TransferRefusal::TargetIsSelf);
    }
    if !self.is_voter(target) {
      return Err(TransferRefusal::TargetNotVoter);
    }
    if let Some(transfer) = self.active_transfer() {
      return Err(TransferRefusal::InFlight {
        target: transfer.target,
      });
    }
    self.transfer = Some(Transfer {
      target,
      term: self.current_term,
      quorum_checks: 0,
      invited: false,
    });
    Ok(())
  }

  /// The transfer in flight: one started in this node's current term while it still leads that term.
  fn active_transfer(&self) -> Option<Transfer> {
    self
      .transfer
      .filter(|transfer| self.role == Role::Leader && transfer.term == self.current_term)
  }

  /// The other voter whose log is furthest along by this leader's replication progress (`match_index`), ties
  /// to the lowest id — the target a leader handing off should pick, since it needs the least catching up
  /// before the invitation can go (thesis §3.10). `None` when this node does not lead or votes alone.
  pub fn most_caught_up_voter(&self) -> Option<HostId> {
    if self.role != Role::Leader {
      return None;
    }
    self
      .all_voters()
      .into_iter()
      .filter(|voter| *voter != self.id)
      .max_by(|left, right| {
        self
          .match_of(*left)
          .cmp(&self.match_of(*right))
          .then(right.cmp(left))
      })
  }

  /// The target of the leadership transfer in flight, if one is.
  pub fn transferring_to(&self) -> Option<HostId> {
    self.active_transfer().map(|transfer| transfer.target)
  }

  /// The invitation to send the transfer's target, once: `Some((target, TimeoutNow))` the first time it is
  /// asked after the target's log matches the leader's (its `match_index` has reached the leader's last
  /// index), `None` before that, after it was sent, and when no transfer is in flight. The caller ships it
  /// with the replication that follows.
  pub fn take_timeout_now(&mut self) -> Option<(HostId, TimeoutNow)> {
    let transfer = self.active_transfer()?;
    if transfer.invited {
      return None;
    }
    let matched = self.match_index.get(&transfer.target).copied().unwrap_or(0);
    if matched < self.last_log_index() {
      return None;
    }
    self.transfer = Some(Transfer {
      invited: true,
      ..transfer
    });
    Some((
      transfer.target,
      TimeoutNow {
        term: self.current_term,
        leader: self.id,
      },
    ))
  }

  /// Handles a [`TimeoutNow`] (thesis §3.10): a voter at the inviting leader's term starts a real election at
  /// once — no pre-vote, since the leader invited it — and returns the vote requests to send. An invitation
  /// from another term, or to a node that leads or does not vote, is ignored (no messages).
  pub fn on_timeout_now(&mut self, invitation: TimeoutNow) -> Vec<RequestVote> {
    if invitation.term != self.current_term || self.role == Role::Leader || !self.is_voter(self.id)
    {
      return Vec::new();
    }
    self.start_election()
  }

  /// Leadership transfers this node started that were aborted at their deadline (thesis §3.10).
  pub fn transfers_aborted(&self) -> u64 {
    self.transfers_aborted
  }

  /// Starts one ReadIndex round after a current-term commit (Raft §6.4; AUD-09). The next
  /// replication/heartbeat to each voter carries this context. A second concurrent read is refused;
  /// callers may share a pending round only for reads that started before that round was sent.
  pub fn begin_read(&mut self) -> Option<u64> {
    if self.role != Role::Leader
      || self.entry_term(self.commit_index) != Some(self.current_term)
      || self.read_round.is_some()
    {
      return None;
    }
    self.read_context = self.read_context.checked_add(1)?;
    // The read index is what this leader has applied and acknowledged, fast commits included: a read after a
    // fast-committed write must see it.
    self.read_round = Some(ReadRound {
      context: self.read_context,
      term: self.current_term,
      index: self.committed_through(),
      config: self.effective_config(),
      confirmed: BTreeSet::from([self.id]),
    });
    Some(self.read_context)
  }

  /// Completes `context` only after a majority has answered that read's round in the same term and
  /// voter configuration. The caller applies through the returned index before answering the read.
  /// The result is consumed: neither this context nor its old replies can authorize a later read.
  pub fn read_index(&mut self, context: u64) -> Option<u64> {
    let read = self.read_round.as_ref()?;
    if read.context != context
      || self.role != Role::Leader
      || read.term != self.current_term
      || read.config != self.effective_config()
      || !self.is_majority(&read.confirmed)
    {
      return None;
    }
    let index = read.index;
    self.read_round = None;
    Some(index)
  }

  /// Releases the single pending read after its caller times out or is cancelled (§4.3). A stale
  /// cancellation cannot remove another caller's newer round.
  pub fn cancel_read(&mut self, context: u64) {
    if self
      .read_round
      .as_ref()
      .is_some_and(|read| read.context == context)
    {
      self.read_round = None;
    }
  }

  /// Begins a membership change to `new_voters` (Raft §6 joint consensus): the node enters a **joint
  /// configuration** where every decision — election, commit, CheckQuorum — needs a majority of both the
  /// old and the new voter sets, so no two disjoint majorities can form across the change. Only the
  /// leader begins one, not while another is in flight, and not while the previous configuration entry
  /// is still uncommitted (one change at a time — Ongaro's thesis §4.1: two configuration entries in
  /// flight could let disjoint majorities form). An empty target is refused (a group cannot vote itself
  /// out of existence), and so is a change while this term's fast track is open: fast votes are counted
  /// against the configuration they were cast under, so a change needs a classic term
  /// (`docs/wip/research/consensus-enhancements.md` §4). Returns whether it started.
  pub fn begin_membership_change(&mut self, new_voters: Vec<HostId>) -> bool {
    let current = self.effective_config();
    if self.role != Role::Leader
      || self.active_transfer().is_some()
      || current.joint.is_some()
      || new_voters.is_empty()
      || self.latest_config_index() > self.commit_index
      || self.open_from > 0
    {
      return false;
    }
    self.read_round = None;
    // Append the joint configuration `C_old,new` as a log entry — it takes effect on append (§6), so the
    // very next quorum check needs a majority of both sets. It replicates like any entry.
    self.leader_append(LogEntry::configuration(
      self.current_term,
      VoterConfig {
        voters: current.voters,
        joint: Some(new_voters),
      },
    ));
    true
  }

  /// Completes a membership change: leaves the joint configuration, adopting the new voter set as the
  /// sole configuration (Raft §6, the transition to `C_new`). Only valid while a change is in flight, and
  /// only once the joint configuration entry has itself **committed** — enforced here, so a caller cannot
  /// leave the joint phase early. Returns whether it completed. A leader not named by `C_new` keeps
  /// leading until that entry commits, then steps down ([`advance_leader_commit`] applies §4.2.2).
  ///
  /// [`advance_leader_commit`]: RaftNode::advance_leader_commit
  pub fn complete_membership_change(&mut self) -> bool {
    let current = self.effective_config();
    let Some(new_voters) = current.joint else {
      return false;
    };
    if self.role != Role::Leader || self.latest_config_index() > self.commit_index {
      return false;
    }
    self.read_round = None;
    // Append the final configuration `C_new` (§6): the change is done once this commits.
    self.leader_append(LogEntry::configuration(
      self.current_term,
      VoterConfig {
        voters: new_voters,
        joint: None,
      },
    ));
    true
  }

  /// Truncates the log from the one-based `index` onward (removing that entry and every later one).
  fn truncate_from(&mut self, index: u64) {
    let keep = usize::try_from(index.saturating_sub(self.snapshot_index).saturating_sub(1))
      .unwrap_or(usize::MAX);
    self.retention_pending |= keep < self.log.len();
    self.log.truncate(keep);
    self.config_entries.split_off(&index);
  }

  /// Appends `entry` to the log, noting its index when it is a configuration entry.
  fn push_entry(&mut self, entry: LogEntry) {
    if entry.config.is_some() {
      self
        .config_entries
        .insert(self.last_log_index().saturating_add(1));
    }
    self.log.push(entry);
  }

  /// The configuration the log's entry at `index` carries, if it is a configuration entry there.
  fn config_at(&self, index: u64) -> Option<VoterConfig> {
    self
      .position(index)
      .and_then(|at| self.log.get(at))
      .and_then(|entry| entry.config.clone())
  }

  /// The configuration below the log: the base, which a compaction or a snapshot folded the log's earlier
  /// configurations into.
  fn base_config(&self) -> VoterConfig {
    VoterConfig {
      voters: self.voters.clone(),
      joint: self.joint.clone(),
    }
  }

  /// A follower's reply with this node's current term.
  fn append_reply(&self, success: bool, match_index: u64, read_context: u64) -> AppendReply {
    AppendReply {
      read_context,
      follower: self.id,
      term: self.current_term,
      success,
      match_index,
      conflict_term: 0,
      conflict_index: 0,
      priority: self.own_priority,
    }
  }

  /// A consistency-check refusal at the leader's previous index `prev` (at or above our commit index), with
  /// the conflict hint of Raft §5.3: when our log does not reach `prev`, no term and the index after our
  /// last entry; otherwise the term we hold at `prev` and the first index of that term's run in our log —
  /// never below the entry after our snapshot, which holds only committed entries.
  fn conflict_reply(&self, prev: u64, read_context: u64) -> AppendReply {
    let (conflict_term, conflict_index) = match self.entry_term(prev) {
      None => (0, self.last_log_index().saturating_add(1)),
      Some(term) => {
        let floor = self.snapshot_index.saturating_add(1);
        let mut first = prev;
        while first > floor && self.entry_term(first.saturating_sub(1)) == Some(term) {
          first = first.saturating_sub(1);
        }
        (term, first)
      }
    };
    AppendReply {
      conflict_term,
      conflict_index,
      ..self.append_reply(false, 0, read_context)
    }
  }

  /// Advances the leader's commit index (Raft §5.4.2): the highest index a majority of voters hold whose
  /// entry is from the **current term**. Earlier-term entries are not committed by replica count alone —
  /// they commit only once a current-term entry above them does — which is the safety subtlety Raft's
  /// figure 8 exposes. A commit that carries the leader's own removal into force steps it down
  /// ([`step_down_if_removed`](RaftNode::step_down_if_removed)).
  fn advance_leader_commit(&mut self) {
    if self.role != Role::Leader {
      return;
    }
    // Raft's rule, §5.3 and §5.4.2: the highest index a majority holds commits when its entry is of this
    // term. A leader's entries of its own term are the end of its log, so no lower index of this term is held
    // by a majority either when that one's entry is older. Until 2026-09-29 each call scanned every index from
    // the log's end down to the commit index, and a proposal cost 20 ms at a 5,000-entry backlog.
    let held = self.quorum_match();
    if held > self.commit_index && self.entry_term(held) == Some(self.current_term) {
      self.retention_pending = true;
      self.commit_index = held;
    }
    // Indices a fast quorum chose are committed once every index below them is: this leader applies and
    // acknowledges them, while its commit index — what followers learn, and what windows prune under — stays
    // classic.
    let last = self.last_log_index();
    let mut through = self.committed_through();
    while through < last && self.fast_chosen.contains(&through.saturating_add(1)) {
      through = through.saturating_add(1);
    }
    self.fast_through = through;
    self.fast_chosen.retain(|index| *index > through);
    // The leader votes on the fast track too, into its own window; its slots go under a classic commit, as a
    // follower's do.
    self.prune_committed();
    self.step_down_if_removed();
  }

  /// How far `voter`'s log matches the leader's: the leader's own last index for itself, else the
  /// follower's tracked `match_index` (nothing for a follower not yet replicated to).
  /// The highest index a majority of every configuration in force holds — for each, the match index at the
  /// place a majority begins among its voters' match indices in descending order — this leader's own being
  /// its last index.
  fn quorum_match(&self) -> u64 {
    let held = |set: &[HostId]| {
      let mut matches: Vec<u64> = set.iter().map(|voter| self.match_of(*voter)).collect();
      matches.sort_unstable_by(|left, right| right.cmp(left));
      matches.get(set.len() / 2).copied().unwrap_or(0)
    };
    let config = self.effective_config();
    let base = held(&config.voters);
    config
      .joint
      .as_ref()
      .map_or(base, |new| base.min(held(new)))
  }

  fn match_of(&self, voter: HostId) -> u64 {
    if voter == self.id {
      self.last_log_index()
    } else {
      self.match_index.get(&voter).copied().unwrap_or(0)
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Shape: an append budget no batch reaches, for the tests of every rule but batching (which pass their
  /// own).
  const UNBOUNDED: usize = usize::MAX;

  const A: HostId = HostId(1);
  const B: HostId = HostId(2);
  const C: HostId = HostId(3);
  const D: HostId = HostId(4);
  const E: HostId = HostId(5);

  /// A fixture with an explicitly uncommitted, uncompacted prefix. Production recovery takes
  /// the complete SavedRaft value, including snapshot and commit position.
  fn node_with_uncommitted_log(
    id: HostId,
    voters: Vec<HostId>,
    term: u64,
    voted_for: Option<HostId>,
    log: Vec<LogEntry>,
  ) -> RaftNode {
    RaftNode::restore(SavedRaft {
      id,
      base: VoterConfig {
        voters,
        joint: None,
      },
      term,
      voted_for,
      log,
      commit_index: 0,
      snapshot_index: 0,
      snapshot_term: 0,
      snapshot_data: Vec::new(),
      window: Vec::new(),
      synced_term: 0,
    })
    .expect("a valid fixture prefix")
  }

  fn request_from(candidate: HostId, term: u64) -> RequestVote {
    RequestVote {
      term,
      candidate,
      last_log_index: 0,
      last_log_term: 0,
    }
  }

  /// A log with one entry per given term (a distinct command each, so entries are unequal).
  fn log_of(terms: &[u64]) -> Vec<LogEntry> {
    terms
      .iter()
      .enumerate()
      .map(|(index, &term)| LogEntry::command(term, vec![u8::try_from(index).unwrap_or(u8::MAX)]))
      .collect()
  }

  /// Drives a candidate to leadership among `voters` by granting it every other voter's vote.
  fn elected_leader(id: HostId, voters: Vec<HostId>) -> RaftNode {
    let mut node = RaftNode::new(id, voters.clone());
    node.start_election();
    for voter in voters {
      if voter != id {
        node.on_vote_reply(VoteReply {
          voter,
          term: node.term(),
          granted: true,
          reports: Vec::new(),
        });
      }
    }
    node
  }

  /// Drives replication from `leader` to `follower` (id `who`), applying each reply, until an append can
  /// no longer be built (the follower needs a snapshot). Returns whether it became stuck; bounded.
  fn replicate_until_stuck(leader: &mut RaftNode, follower: &mut RaftNode, who: HostId) -> bool {
    for _ in 0..8 {
      let Some(append) = leader.replicate_to(who, UNBOUNDED) else {
        return true;
      };
      let reply = follower.on_append_entries(append);
      leader.on_append_reply(reply);
    }
    false
  }

  /// AC-8.1, §4.8, AUD-07: a damaged retained prefix cannot become a voter; a complete
  /// wire round-trip preserves the vote and rejects a competing candidate in the same term.
  #[test]
  fn damaged_voter_publications_are_refused_before_voting() {
    use slates_wire::Wire;
    let mut voter = RaftNode::new(A, vec![A, B, C]);
    assert!(
      voter
        .on_request_vote(RequestVote {
          term: 7,
          candidate: B,
          last_log_index: 0,
          last_log_term: 0,
        })
        .granted
    );
    let saved = voter.saved();
    let bytes = saved.to_bytes();
    for end in 0..bytes.len() {
      assert!(SavedRaft::from_bytes(&bytes[..end]).is_err());
    }
    let mut recovered = RaftNode::restore(SavedRaft::from_bytes(&bytes).unwrap()).unwrap();
    assert!(
      !recovered
        .on_request_vote(RequestVote {
          term: 7,
          candidate: C,
          last_log_index: 0,
          last_log_term: 0,
        })
        .granted
    );
    let mut damaged = saved.clone();
    damaged.commit_index = 1;
    assert_eq!(
      RaftNode::restore(damaged).err(),
      Some(RaftRecoveryError::InvalidCommitIndex)
    );
    let mut damaged = saved.clone();
    damaged.base.voters.push(A);
    assert_eq!(
      RaftNode::restore(damaged).err(),
      Some(RaftRecoveryError::InvalidVoters)
    );
    let mut damaged = saved.clone();
    damaged.snapshot_index = 1;
    assert_eq!(
      RaftNode::restore(damaged).err(),
      Some(RaftRecoveryError::InvalidSnapshot)
    );
    let mut damaged = saved;
    damaged.log.push(LogEntry::command(8, vec![1]));
    assert_eq!(
      RaftNode::restore(damaged).err(),
      Some(RaftRecoveryError::InvalidLogTerm)
    );
  }

  /// AC-8.1, §4.8, AUD-07: retain a vote, compact a committed command and change membership.
  /// Restore the prefix, snapshot and effective voters together, and refuse a competing vote.
  #[test]
  fn recovery_preserves_the_vote_snapshot_and_membership_with_the_log() {
    let mut node = RaftNode::new(A, vec![A]);
    node.start_election();
    assert!(node.append_command(b"committed before restart".to_vec()));
    assert!(node.compact(1, b"state at index one".to_vec()));
    assert!(node.begin_membership_change(vec![A, B]));
    let mut follower = RaftNode::new(B, vec![A]);
    // Catch up from the compacted prefix before accepting the joint configuration.
    let snapshot = node.install_snapshot_for(B).unwrap();
    node.on_install_snapshot_reply(follower.on_install_snapshot(snapshot));
    let append = node.replicate_to(B, UNBOUNDED).unwrap();
    node.on_append_reply(follower.on_append_entries(append));
    let saved = node.saved();
    let restored = RaftNode::restore(saved.clone()).unwrap();
    assert_eq!(restored.saved(), saved);
    assert_eq!(restored.role(), Role::Follower);
    assert!(!restored.is_leader());
    let mut restored = restored;
    assert!(
      !restored
        .on_request_vote(RequestVote {
          term: 1,
          candidate: B,
          last_log_index: 2,
          last_log_term: 1,
        })
        .granted,
      "the recovered self-vote must still exclude another candidate"
    );
    assert_eq!(restored.all_voters(), vec![A, B]);
  }

  /// AC-8.1, §4.8 restart-as-join; AUD-07: a fresh replacement is outside the existing voter
  /// configuration. It may receive replication, but neither a vote request nor direct campaign
  /// entrypoint may let it supply a vote before a membership entry admits it.
  #[test]
  fn a_learner_neither_grants_votes_nor_campaigns_before_admission() {
    let mut learner = RaftNode::new(D, vec![A, B, C]);
    let request = RequestVote {
      term: 7,
      candidate: A,
      last_log_index: 0,
      last_log_term: 0,
    };
    assert!(!learner.on_request_vote(request).granted);
    assert!(
      !learner
        .on_pre_vote(PreVote {
          term: 8,
          candidate: A,
          last_log_index: 0,
          last_log_term: 0,
        })
        .granted
    );
    assert!(learner.start_election().is_empty());
    assert_eq!(learner.role(), Role::Follower);
  }

  /// A single-voter group elects itself: its vote reaches a majority of one, so it becomes
  /// leader for term 1 without sending messages (the `f = 0` degenerate).
  #[test]
  fn a_single_voter_elects_itself_leader() {
    let mut node = RaftNode::new(A, vec![A]);
    let requests = node.start_election();
    assert!(requests.is_empty(), "a lone voter sends no requests");
    assert!(node.is_leader(), "and is immediately leader");
    assert_eq!(node.term(), 1);
  }

  /// A candidate that gathers a majority of votes becomes leader; among three voters, its own vote plus
  /// one granted reply is the majority.
  #[test]
  fn a_candidate_becomes_leader_at_a_majority() {
    let mut node = RaftNode::new(A, vec![A, B, C]);
    let requests = node.start_election();
    assert_eq!(requests.len(), 2, "a request to each other voter");
    assert_eq!(node.role(), Role::Candidate);

    node.on_vote_reply(VoteReply {
      voter: B,
      term: 1,
      granted: true,
      reports: Vec::new(),
    });
    assert!(node.is_leader(), "self plus one of three is a majority");
  }

  /// A candidate short of a majority stays a candidate: among five voters, its own vote plus one is not
  /// enough.
  #[test]
  fn a_split_vote_stays_a_candidate() {
    let mut node = RaftNode::new(A, vec![A, B, C, D, E]);
    node.start_election();
    node.on_vote_reply(VoteReply {
      voter: B,
      term: 1,
      granted: true,
      reports: Vec::new(),
    });
    assert_eq!(
      node.role(),
      Role::Candidate,
      "two of five is not a majority"
    );
    node.on_vote_reply(VoteReply {
      voter: C,
      term: 1,
      granted: true,
      reports: Vec::new(),
    });
    assert!(node.is_leader(), "three of five is");
  }

  /// A voter grants its vote once per term: having voted for one candidate, it denies another in the
  /// same term, but grants the one it already voted for (idempotent retry).
  #[test]
  fn a_vote_is_granted_once_per_term() {
    let mut node = RaftNode::new(A, vec![A, B, C]);
    assert!(
      node.on_request_vote(request_from(B, 1)).granted,
      "first candidate wins the vote"
    );
    assert!(
      !node.on_request_vote(request_from(C, 1)).granted,
      "a second candidate is denied"
    );
    assert!(
      node.on_request_vote(request_from(B, 1)).granted,
      "the same candidate is granted again (a lost reply retried)"
    );
  }

  /// A request under a newer term steps a candidate down to a follower and can win its vote (its own
  /// candidacy is abandoned for the newer term).
  #[test]
  fn a_newer_term_steps_a_candidate_down_and_can_win_its_vote() {
    let mut node = RaftNode::new(A, vec![A, B, C]);
    node.start_election(); // A is a candidate at term 1
    assert_eq!(node.role(), Role::Candidate);

    let reply = node.on_request_vote(request_from(B, 2));
    assert!(reply.granted, "the newer-term candidate wins the vote");
    assert_eq!(node.role(), Role::Follower, "and A is now a follower");
    assert_eq!(node.term(), 2, "at the newer term");
  }

  /// A vote request under a stale term is denied, and the reply carries our current term so the stale
  /// candidate learns it is behind.
  #[test]
  fn a_stale_term_request_is_denied() {
    let mut node = node_with_uncommitted_log(A, vec![A, B, C], 5, None, Vec::new());
    let reply = node.on_request_vote(request_from(B, 3));
    assert!(!reply.granted, "a term-3 request is stale at term 5");
    assert_eq!(reply.term, 5, "the reply reports the current term");
  }

  /// The election restriction (Raft §5.4.1): a candidate whose log is less up-to-date than ours is
  /// denied, while one at least as up-to-date is granted — so a leader's log never omits a committed
  /// entry.
  #[test]
  fn a_less_up_to_date_candidate_is_denied() {
    // Our last log entry is at index 3, term 2.
    let mut node = node_with_uncommitted_log(A, vec![A, B, C], 2, None, log_of(&[1, 1, 2]));

    // A candidate at the same term but a shorter log is denied.
    let behind = RequestVote {
      term: 3,
      candidate: B,
      last_log_index: 2,
      last_log_term: 2,
    };
    assert!(
      !node.on_request_vote(behind).granted,
      "a shorter log at the same term is not current"
    );

    // A fresh node at the same state grants a candidate with a later last-log term despite a shorter log.
    let mut node = node_with_uncommitted_log(A, vec![A, B, C], 2, None, log_of(&[1, 1, 2]));
    let ahead = RequestVote {
      term: 3,
      candidate: C,
      last_log_index: 1,
      last_log_term: 3,
    };
    assert!(
      node.on_request_vote(ahead).granted,
      "a later last-log term is more up-to-date"
    );
  }

  /// A single-voter leader commits its own appends at once — a majority of one (the `f = 0` degenerate
  /// of log replication).
  #[test]
  fn a_lone_leader_commits_its_own_appends() {
    let mut node = elected_leader(A, vec![A]);
    assert!(node.is_leader());
    assert!(node.append_command(b"cfg-1".to_vec()));
    assert_eq!(
      node.commit_index(),
      1,
      "a lone voter's append is immediately committed"
    );
    assert!(node.append_command(b"cfg-2".to_vec()));
    assert_eq!(node.commit_index(), 2);
    assert_eq!(
      node.committed_entries().len(),
      2,
      "both appends are in the committed prefix"
    );
  }

  /// A leader replicates an append to a follower; once a majority (leader plus one of three) holds the
  /// entry, it commits.
  #[test]
  fn an_append_commits_once_a_majority_replicates_it() {
    let mut leader = elected_leader(A, vec![A, B, C]);
    leader.append_command(b"cfg-1".to_vec());
    assert_eq!(
      leader.commit_index(),
      0,
      "not committed until a majority holds it"
    );

    let to_b = leader
      .replicate_to(B, UNBOUNDED)
      .expect("leader replicates");
    assert_eq!(to_b.entries.len(), 1);
    let mut follower = RaftNode::new(B, vec![A, B, C]);
    let reply = follower.on_append_entries(to_b);
    assert!(reply.success, "the follower accepts the append");
    leader.on_append_reply(reply);

    assert_eq!(
      leader.commit_index(),
      1,
      "leader plus one follower is a majority of three"
    );
    assert_eq!(follower.last_log_index(), 1, "the follower holds the entry");
  }

  /// A follower whose log does not match at the previous position rejects the append; the leader backs up
  /// `next_index` and retries from earlier until the follower's log is repaired to agree (the log-repair
  /// loop, §5.3).
  #[test]
  fn a_mismatched_follower_is_repaired_by_backing_up() {
    // A leader for a fresh term over a two-entry log, plus one new current-term entry.
    let mut leader = node_with_uncommitted_log(A, vec![A, B, C], 3, None, log_of(&[3, 3]));
    leader.start_election(); // term 4
    leader.on_vote_reply(VoteReply {
      voter: B,
      term: leader.term(),
      granted: true,
      reports: Vec::new(),
    });
    assert!(leader.is_leader());
    leader.append_command(b"cfg-new".to_vec()); // index 3, term 4

    // A follower with a single conflicting entry (term 1) at index 1.
    let mut follower = node_with_uncommitted_log(B, vec![A, B, C], 1, None, log_of(&[1]));

    let first = leader.replicate_to(B, UNBOUNDED).expect("append");
    let reply = follower.on_append_entries(first);
    assert!(!reply.success, "a mismatched previous entry is rejected");
    leader.on_append_reply(reply);

    for _ in 0..5 {
      let append = leader.replicate_to(B, UNBOUNDED).expect("append");
      let reply = follower.on_append_entries(append);
      leader.on_append_reply(reply);
      if reply.success {
        break;
      }
    }
    assert_eq!(
      follower.last_log_index(),
      leader.last_log_index(),
      "the follower's log is repaired to match the leader's"
    );
    assert_eq!(
      follower.last_log_term(),
      4,
      "including the leader's newest entry"
    );
  }

  /// The commit-safety rule (Raft §5.4.2): a leader does not commit an entry from an earlier term by
  /// replica count alone — committing a current-term entry is what carries the earlier ones with it.
  #[test]
  fn an_earlier_term_entry_is_not_committed_by_count_alone() {
    // A leader for term 5 holding one entry left over from term 2 (index 1).
    let mut leader = node_with_uncommitted_log(A, vec![A, B, C], 4, None, log_of(&[2]));
    leader.start_election(); // term 5
    leader.on_vote_reply(VoteReply {
      voter: B,
      term: leader.term(),
      granted: true,
      reports: Vec::new(),
    });
    assert!(leader.is_leader());

    // A majority replicates the old (term-2) entry — it must NOT be committed by count alone.
    let mut follower = node_with_uncommitted_log(B, vec![A, B, C], 5, None, Vec::new());
    let append = leader.replicate_to(B, UNBOUNDED).expect("append");
    let reply = follower.on_append_entries(append);
    leader.on_append_reply(reply);
    assert_eq!(
      leader.commit_index(),
      0,
      "an earlier-term entry is not committed by replica count"
    );

    // Appending and replicating a current-term entry commits both together.
    leader.append_command(b"cfg-5".to_vec()); // index 2, term 5
    let append = leader.replicate_to(B, UNBOUNDED).expect("append");
    let reply = follower.on_append_entries(append);
    leader.on_append_reply(reply);
    assert_eq!(
      leader.commit_index(),
      2,
      "committing the current-term entry carries the earlier one"
    );
  }

  /// A lone voter's pre-vote round carries at once and proceeds straight to a real election, so it leads
  /// (the `f = 0` degenerate of PreVote).
  #[test]
  fn a_solo_node_pre_elects_and_leads() {
    let mut node = RaftNode::new(A, vec![A]);
    let pre_votes = node.on_election_timeout();
    assert!(pre_votes.is_empty(), "a lone voter sends no pre-votes");
    assert!(node.is_leader(), "and proceeds straight to leadership");
    assert_eq!(node.term(), 1);
  }

  /// The anti-disruption property (Raft §9.6): a node that has heard from a leader refuses a pre-vote —
  /// even one at a far higher term — and, crucially, its own term is left untouched, so a partitioned,
  /// term-inflated node that rejoins cannot force the healthy leader to step down.
  #[test]
  fn a_partitioned_node_cannot_disrupt_a_node_with_a_leader() {
    let mut node = RaftNode::new(B, vec![A, B, C]);
    // B hears a heartbeat from leader A at term 1.
    node.on_append_entries(AppendEntries {
      read_context: 0,
      term: 1,
      leader: A,
      prev_log_index: 0,
      prev_log_term: 0,
      entries: Vec::new(),
      leader_commit: 0,
      priorities: Vec::new(),
      sync_index: 0,
      open_from: 0,
    });
    assert_eq!(node.term(), 1);

    // A partitioned node with an inflated term asks for a pre-vote.
    let reply = node.on_pre_vote(PreVote {
      term: 10,
      candidate: C,
      last_log_index: 0,
      last_log_term: 0,
    });
    assert!(
      !reply.granted,
      "a node with a live leader refuses the pre-vote"
    );
    assert_eq!(
      node.term(),
      1,
      "and its term is not inflated by the pre-vote"
    );
  }

  /// A pre-vote majority starts the real election: a pre-candidate that gathers a majority of pre-votes
  /// increments its term and issues real vote requests.
  #[test]
  fn pre_votes_from_a_majority_start_a_real_election() {
    let mut node = RaftNode::new(A, vec![A, B, C]);
    let pre_votes = node.on_election_timeout();
    assert_eq!(pre_votes.len(), 2, "a pre-vote to each other voter");
    assert_eq!(node.role(), Role::PreCandidate);
    assert_eq!(
      node.term(),
      0,
      "the term is not inflated during the pre-vote round"
    );

    let requests = node.on_pre_vote_reply(PreVoteReply {
      voter: B,
      term: 1,
      granted: true,
    });
    let requests = requests.expect("a pre-vote majority starts the real election");
    assert_eq!(requests.len(), 2, "real vote requests are issued");
    assert_eq!(node.role(), Role::Candidate);
    assert_eq!(node.term(), 1, "now the term advances");
  }

  /// A candidate whose term is behind is refused a pre-vote even by a leaderless peer — its pre-vote term
  /// does not exceed the peer's, so it could not win a real election either.
  #[test]
  fn a_behind_candidate_is_refused_a_pre_vote() {
    // A leaderless peer at term 5.
    let node = node_with_uncommitted_log(A, vec![A, B, C], 5, None, Vec::new());
    let reply = node.on_pre_vote(PreVote {
      term: 3,
      candidate: B,
      last_log_index: 0,
      last_log_term: 0,
    });
    assert!(
      !reply.granted,
      "a pre-vote term not ahead of ours is refused"
    );
  }

  /// CheckQuorum (Raft §6.2): a leader that goes a whole window without contact from a majority steps
  /// down. The first check after election still counts the electing votes; a second, with no replies
  /// since, finds only itself and relinquishes leadership.
  #[test]
  fn a_leader_without_a_quorum_steps_down() {
    let mut leader = elected_leader(A, vec![A, B, C]);
    assert!(leader.is_leader());

    leader.check_quorum(); // window one: still counts the electing majority
    assert!(
      leader.is_leader(),
      "the freshly-won leader is not stepped down"
    );

    leader.check_quorum(); // window two: no contact since — steps down
    assert_eq!(
      leader.role(),
      Role::Follower,
      "a leader cut off from a majority steps down"
    );
  }

  /// The leader-redirection hint ([`RaftNode::leader`]): a leader reports itself, a follower learns the leader
  /// from an accepted append, and campaigning forgets it. A hint for redirecting an operator command to the
  /// leader — never a safety input.
  #[test]
  fn the_leader_hint_tracks_the_current_leader() {
    // A leader reports itself.
    let leader = elected_leader(A, vec![A, B, C]);
    assert_eq!(leader.leader(), Some(A));

    // A fresh follower knows no leader until it accepts an append, then reports its sender.
    let mut node = RaftNode::new(B, vec![A, B, C]);
    assert_eq!(node.leader(), None);
    node.on_append_entries(AppendEntries {
      read_context: 0,
      term: 1,
      leader: A,
      prev_log_index: 0,
      prev_log_term: 0,
      entries: Vec::new(),
      leader_commit: 0,
      priorities: Vec::new(),
      sync_index: 0,
      open_from: 0,
    });
    assert_eq!(
      node.leader(),
      Some(A),
      "a follower learns the leader from its append"
    );

    // Campaigning forgets the hint, so a candidate never redirects to a stale leader.
    node.on_election_timeout();
    assert_eq!(node.leader(), None, "a campaigning node forgets its leader");
  }

  /// A leader that keeps hearing from a follower stays leader across checks — the contact refreshes the
  /// window.
  #[test]
  fn a_leader_with_a_quorum_stays() {
    let mut leader = elected_leader(A, vec![A, B, C]);
    leader.check_quorum(); // resets the window

    // A follower replies within the new window, so the leader is in contact with a majority.
    leader.on_append_reply(AppendReply {
      read_context: 0,
      follower: B,
      term: leader.term(),
      success: true,
      match_index: 0,
      conflict_term: 0,
      conflict_index: 0,
      priority: ElectionPriority::default(),
    });
    leader.check_quorum();
    assert!(
      leader.is_leader(),
      "a leader in contact with a majority stays"
    );
  }

  /// A lone leader never steps down under CheckQuorum — it is always its own majority (the `f = 0`
  /// degenerate).
  #[test]
  fn a_solo_leader_never_steps_down() {
    let mut leader = elected_leader(A, vec![A]);
    for _ in 0..3 {
      leader.check_quorum();
    }
    assert!(
      leader.is_leader(),
      "a single voter is always its own quorum"
    );
  }

  /// ReadIndex (Raft §6.4): a leader serves a read only after committing in its current term. A lone
  /// leader has no read index until it commits an entry; then the read index is its commit index.
  /// Delivers `invitation` to `target` and runs the election it starts: one vote request per other voter,
  /// each answered, every reply folded by the target.
  fn run_invited_election(
    target: &mut RaftNode,
    invitation: TimeoutNow,
    voters: &mut [&mut RaftNode],
  ) {
    let requests = target.on_timeout_now(invitation);
    assert_eq!(
      requests.len(),
      voters.len(),
      "one vote request per other voter"
    );
    for (voter, request) in voters.iter_mut().zip(requests) {
      let reply = voter.on_request_vote(request);
      target.on_vote_reply(reply);
    }
  }

  /// Thesis §3.10 (`docs/wip/research/consensus-enhancements.md` §3.2): a leader transferring leadership
  /// refuses proposals, withholds the invitation until the target's log matches its own, and sends it once.
  #[test]
  fn a_transferring_leader_refuses_proposals_and_invites_a_caught_up_target_once() {
    let voters = vec![A, B, C];
    let mut a = elected_leader(A, voters.clone());
    let mut b = RaftNode::new(B, voters);
    assert!(a.append_command(b"one".to_vec()));
    assert_eq!(a.transfer_leadership(B), Ok(()));
    assert_eq!(a.transferring_to(), Some(B));
    assert!(
      !a.append_command(b"refused".to_vec()),
      "no proposals while transferring"
    );
    assert!(
      a.take_timeout_now().is_none(),
      "no invitation before the target has caught up"
    );
    replicate_until_stuck(&mut a, &mut b, B);
    let invited = a.take_timeout_now().map(|(target, _)| target);
    assert_eq!(invited, Some(B), "the caught-up target is invited");
    assert!(
      a.take_timeout_now().is_none(),
      "the invitation is sent once"
    );
  }

  /// A leader A holding two entries mid-transfer to B, with B caught up: the three nodes and the invitation.
  fn caught_up_transfer() -> (RaftNode, RaftNode, RaftNode, TimeoutNow) {
    let voters = vec![A, B, C];
    let mut a = elected_leader(A, voters.clone());
    let mut b = RaftNode::new(B, voters.clone());
    let c = RaftNode::new(C, voters);
    assert!(a.append_command(b"one".to_vec()));
    assert!(a.append_command(b"two".to_vec()));
    assert_eq!(a.transfer_leadership(B), Ok(()));
    replicate_until_stuck(&mut a, &mut b, B);
    let (_, invitation) = a.take_timeout_now().expect("B has caught up");
    (a, b, c, invitation)
  }

  /// Thesis §3.10: the invited voter campaigns at once (no pre-vote, no election timeout) and wins; the old
  /// leader steps down on its vote request, which ends the transfer; the new leader holds every entry the
  /// old one had.
  #[test]
  fn the_invited_voter_wins_and_the_old_leader_steps_down() {
    let (mut a, mut b, mut c, invitation) = caught_up_transfer();
    run_invited_election(&mut b, invitation, &mut [&mut a, &mut c]);
    assert!(b.is_leader(), "the invited voter won its election");
    assert_eq!(a.role(), Role::Follower, "the old leader stepped down");
    assert_eq!(
      a.transferring_to(),
      None,
      "the transfer ended with the leadership"
    );
    assert_eq!(b.last_log_index(), a.last_log_index());
    assert_eq!(b.last_log_term(), a.last_log_term());
  }

  /// Thesis §3.10: a transfer that does not complete — its target never catches up — is aborted at the
  /// second CheckQuorum tick (counted), and the leader accepts proposals again. The leader keeps its
  /// quorum through the other voter all along, so the abort is the transfer's deadline, not a step-down.
  #[test]
  fn a_transfer_that_does_not_complete_is_aborted_and_proposals_resume() {
    let mut a = elected_leader(A, vec![A, B, C]);
    assert_eq!(a.transfer_leadership(C), Ok(()));
    assert!(!a.append_command(b"held".to_vec()));
    for tick in 1..=2 {
      a.on_append_reply(AppendReply {
        read_context: 0,
        follower: B,
        term: a.term(),
        success: true,
        match_index: 0,
        conflict_term: 0,
        conflict_index: 0,
        priority: ElectionPriority::default(),
      });
      a.check_quorum();
      assert!(
        a.is_leader(),
        "the leader keeps its quorum through tick {tick}"
      );
    }
    assert_eq!(a.transferring_to(), None, "aborted at the second tick");
    assert_eq!(a.transfers_aborted(), 1);
    assert!(a.append_command(b"resumed".to_vec()), "proposals resume");
  }

  /// Thesis §3.10: a transfer is refused by type — from a non-leader, to the leader itself, to a node that
  /// does not vote, and while another is in flight.
  #[test]
  fn a_transfer_is_refused_by_type() {
    let voters = vec![A, B, C];
    let mut follower = RaftNode::new(B, voters.clone());
    assert_eq!(
      follower.transfer_leadership(A),
      Err(TransferRefusal::NotLeader)
    );
    let mut a = elected_leader(A, voters);
    assert_eq!(a.transfer_leadership(A), Err(TransferRefusal::TargetIsSelf));
    assert_eq!(
      a.transfer_leadership(D),
      Err(TransferRefusal::TargetNotVoter)
    );
    assert_eq!(a.transfer_leadership(B), Ok(()));
    assert_eq!(
      a.transfer_leadership(C),
      Err(TransferRefusal::InFlight { target: B })
    );
  }

  /// Thesis §3.10, the drain's target: the most caught-up other voter by the leader's replication progress,
  /// ties to the lowest id; no target for a non-leader or a lone voter.
  #[test]
  fn the_most_caught_up_voter_is_the_drain_target() {
    let mut leader = elected_leader(A, vec![A, B, C]);
    assert!(leader.append_command(b"one".to_vec()));
    assert!(leader.append_command(b"two".to_vec()));
    assert_eq!(
      leader.most_caught_up_voter(),
      Some(B),
      "a tie goes to the lowest id"
    );
    let term = leader.term();
    for (follower, match_index) in [(B, 1), (C, 2)] {
      leader.on_append_reply(AppendReply {
        read_context: 0,
        follower,
        term,
        success: true,
        match_index,
        conflict_term: 0,
        conflict_index: 0,
        priority: ElectionPriority::default(),
      });
    }
    assert_eq!(
      leader.most_caught_up_voter(),
      Some(C),
      "the furthest along wins"
    );
    assert_eq!(RaftNode::new(B, vec![A, B, C]).most_caught_up_voter(), None);
    assert_eq!(elected_leader(A, vec![A]).most_caught_up_voter(), None);
  }

  /// Thesis §3.10: an invitation from another term is ignored — a delayed `TimeoutNow` cannot start an
  /// election after the leadership it came from has moved on.
  #[test]
  fn an_invitation_from_another_term_is_ignored() {
    let mut b = RaftNode::new(B, vec![A, B, C]);
    b.observe_term(3);
    assert!(
      b.on_timeout_now(TimeoutNow { term: 2, leader: A })
        .is_empty()
    );
    assert_eq!(b.role(), Role::Follower);
    assert_eq!(b.term(), 3);
  }

  #[test]
  fn a_leader_serves_a_read_index_after_committing_in_its_term() {
    let mut leader = elected_leader(A, vec![A]);
    assert_eq!(
      leader.begin_read(),
      None,
      "no read before a current-term commit"
    );

    leader.append_command(b"cfg-1".to_vec()); // commits at once (f = 0)
    let read = leader
      .begin_read()
      .expect("current-term commit starts a read");
    assert_eq!(
      leader.read_index(read),
      Some(1),
      "the read index is the current commit index"
    );
  }

  /// A non-leader never provides a read index — only the leader may serve a linearizable read.
  #[test]
  fn a_non_leader_has_no_read_index() {
    let mut follower = RaftNode::new(B, vec![A, B, C]);
    assert_eq!(follower.begin_read(), None);
  }

  /// AC-2.5, §4.8 ReadSafety; AUD-09: after A's commit, isolate A and elect B, then commit a
  /// later value through B and C. A has not run CheckQuorum yet. Its old contacts must not authorize
  /// a new read of the superseded value.
  #[test]
  fn historical_contacts_cannot_confirm_a_read_after_a_new_leader_commits() {
    let mut old = elected_leader(A, vec![A, B, C]);
    let mut successor = RaftNode::new(B, vec![A, B, C]);
    let mut third = RaftNode::new(C, vec![A, B, C]);
    old.append_command(b"old".to_vec());
    let append = old.replicate_to(B, UNBOUNDED).unwrap();
    old.on_append_reply(successor.on_append_entries(append));
    let election = successor.start_election();
    successor.on_vote_reply(third.on_request_vote(election[0]));
    assert!(successor.is_leader());
    successor.append_command(b"new".to_vec());
    let append = successor.replicate_to(C, UNBOUNDED).unwrap();
    successor.on_append_reply(third.on_append_entries(append));
    let append = successor.replicate_to(C, UNBOUNDED).unwrap();
    successor.on_append_reply(third.on_append_entries(append));
    assert_eq!(successor.commit_index(), 2);
    let read = old
      .begin_read()
      .expect("old leader has not stepped down yet");
    assert_eq!(
      old.read_index(read),
      None,
      "pre-read contacts cannot prove current read authority"
    );
  }

  /// AC-2.5, §4.8 ReadSafety; AUD-09: old heartbeats and completed read rounds cannot confirm a
  /// new read. The follower's reply must echo the new context; a result is consumed exactly once.
  #[test]
  fn each_read_needs_its_own_confirmation_round() {
    let mut leader = elected_leader(A, vec![A, B, C]);
    let mut follower = RaftNode::new(B, vec![A, B, C]);
    leader.append_command(b"value".to_vec());
    let old = follower.on_append_entries(leader.replicate_to(B, UNBOUNDED).unwrap());
    leader.on_append_reply(old);
    let first = leader.begin_read().unwrap();
    assert_eq!(
      leader.begin_read(),
      None,
      "one pending round is the admission bound"
    );
    leader.on_append_reply(old);
    assert_eq!(leader.read_index(first), None);
    let confirmed = follower.on_append_entries(leader.replicate_to(B, UNBOUNDED).unwrap());
    leader.on_append_reply(confirmed);
    assert_eq!(leader.read_index(first), Some(1));
    assert_eq!(
      leader.read_index(first),
      None,
      "a proof cannot authorize another read"
    );
    let second = leader.begin_read().unwrap();
    leader.on_append_reply(confirmed);
    leader.cancel_read(first);
    assert_eq!(
      leader.read_index(second),
      None,
      "old replies and cancellation cannot complete the new round"
    );
    let append = leader.replicate_to(B, UNBOUNDED).unwrap();
    leader.on_append_reply(follower.on_append_entries(append));
    assert_eq!(leader.read_index(second), Some(1));
  }

  /// AC-2.5, §4.8 ReadSafety; AUD-09: two voters of five are a minority even with duplicate or
  /// foreign replies. Changing the voter configuration cancels the pending proof.
  #[test]
  fn read_confirmation_counts_distinct_current_voters_and_cancels_on_reconfiguration() {
    let voters = vec![A, B, C, D, E];
    let mut leader = elected_leader(A, voters.clone());
    leader.append_command(b"value".to_vec());
    let mut second = RaftNode::new(B, voters.clone());
    let mut third = RaftNode::new(C, voters);
    let append = leader.replicate_to(B, UNBOUNDED).unwrap();
    leader.on_append_reply(second.on_append_entries(append));
    let append = leader.replicate_to(C, UNBOUNDED).unwrap();
    leader.on_append_reply(third.on_append_entries(append));
    let read = leader.begin_read().unwrap();
    let reply = second.on_append_entries(leader.replicate_to(B, UNBOUNDED).unwrap());
    leader.on_append_reply(reply);
    leader.on_append_reply(reply);
    leader.on_append_reply(AppendReply {
      follower: HostId(999),
      ..reply
    });
    assert_eq!(leader.read_index(read), None);
    assert!(leader.begin_membership_change(vec![A, B, C]));
    let append = leader.replicate_to(C, UNBOUNDED).unwrap();
    leader.on_append_reply(third.on_append_entries(append));
    assert_eq!(
      leader.read_index(read),
      None,
      "the old configuration's proof is cancelled"
    );
    let replacement = leader.begin_read().unwrap();
    leader.cancel_read(replacement);
    assert!(
      leader.begin_read().is_some(),
      "a cancelled caller releases the round"
    );
  }

  /// The §6.4 safety: a leader that has only an inherited (earlier-term) commit index cannot serve a
  /// linearizable read until it commits an entry in its own term — so it never serves a read at a commit
  /// index it has not confirmed under its own leadership.
  #[test]
  fn a_leader_without_a_current_term_commit_has_no_read_index() {
    // Elected at a fresh term over an old-term log; recovered resets the commit index to zero.
    let mut leader = node_with_uncommitted_log(A, vec![A, B, C], 3, None, log_of(&[3]));
    leader.start_election(); // term 4
    leader.on_vote_reply(VoteReply {
      voter: B,
      term: leader.term(),
      granted: true,
      reports: Vec::new(),
    });
    assert!(leader.is_leader());
    assert_eq!(
      leader.begin_read(),
      None,
      "no read until a term-4 entry commits"
    );

    // Commit a current-term entry with a majority (a follower that already holds the term-3 prefix).
    leader.append_command(b"cfg-4".to_vec());
    let mut follower = node_with_uncommitted_log(B, vec![A, B, C], 4, None, log_of(&[3]));
    let append = leader.replicate_to(B, UNBOUNDED).expect("append");
    let reply = follower.on_append_entries(append);
    assert!(
      reply.success,
      "the follower with the matching prefix accepts the append"
    );
    leader.on_append_reply(reply);
    let read = leader
      .begin_read()
      .expect("current-term commit starts a read");
    assert_eq!(leader.read_index(read), None);
    let append = leader.replicate_to(B, UNBOUNDED).unwrap();
    leader.on_append_reply(follower.on_append_entries(append));
    assert_eq!(
      leader.read_index(read),
      Some(2),
      "a term-4 commit enables the read at index 2"
    );
  }

  /// Joint consensus (Raft §6): during a membership change a commit needs a majority of BOTH the old and
  /// the new voter sets. A majority of the old configuration alone does not commit; only when both
  /// configurations hold the entry does it commit — so no two disjoint majorities can form across a
  /// change.
  #[test]
  fn a_joint_change_needs_a_majority_of_both_configurations() {
    let mut leader = elected_leader(A, vec![A, B, C]);
    assert!(
      leader.begin_membership_change(vec![C, D, E]),
      "the leader enters the joint configuration"
    );
    assert!(leader.in_joint_configuration());

    leader.append_command(b"x".to_vec());
    let term = leader.term();
    let reply = |follower| AppendReply {
      read_context: 0,
      follower,
      term,
      success: true,
      match_index: 1,
      conflict_term: 0,
      conflict_index: 0,
      priority: ElectionPriority::default(),
    };

    // B is a majority of the old set {A,B,C} together with A, but holds no majority of the new set.
    leader.on_append_reply(reply(B));
    assert_eq!(
      leader.commit_index(),
      0,
      "a majority of the old configuration alone does not commit during a joint change"
    );

    // C and D bring a majority of the new set {C,D,E} too (with A and B, still a majority of the old).
    leader.on_append_reply(reply(C));
    leader.on_append_reply(reply(D));
    assert_eq!(
      leader.commit_index(),
      1,
      "a majority of both configurations commits"
    );
  }

  /// Completing a change leaves the joint configuration for the new voter set alone, after which a
  /// majority is measured against the new set only. It completes only once the joint entry has committed
  /// under both sets (the core enforces the gate).
  #[test]
  fn completing_a_change_adopts_the_new_configuration() {
    let mut leader = elected_leader(A, vec![A, B, C]);
    leader.begin_membership_change(vec![C, D, E]);
    assert!(leader.in_joint_configuration());

    // Commit the joint entry: B with A carries the old set {A,B,C}; C and D carry the new set {C,D,E}.
    let term = leader.term();
    for follower in [B, C, D] {
      leader.on_append_reply(AppendReply {
        read_context: 0,
        follower,
        term,
        success: true,
        match_index: 1,
        conflict_term: 0,
        conflict_index: 0,
        priority: ElectionPriority::default(),
      });
    }
    assert_eq!(leader.commit_index(), 1, "the joint entry committed");
    assert!(leader.complete_membership_change(), "the change completes");
    assert!(!leader.in_joint_configuration(), "no longer joint");
    assert_eq!(
      leader.all_voters(),
      vec![C, D, E],
      "the new configuration is the sole one"
    );
  }

  /// Only a leader may begin a membership change — a follower cannot.
  #[test]
  fn only_a_leader_begins_a_change() {
    let mut follower = RaftNode::new(A, vec![A, B, C]);
    assert!(!follower.begin_membership_change(vec![C, D, E]));
    assert!(!follower.in_joint_configuration());
  }

  /// A leader whose own removal commits **steps down** (Ongaro's thesis §4.2.2) — and only then: while the
  /// change is joint the old configuration still names it and it keeps leading. Both configurations commit
  /// the joint entry; `C_new` commits under the new majority alone; the leader is then a follower, no
  /// longer a voter, and never campaigns again.
  #[test]
  fn a_removed_leader_steps_down_once_its_removal_commits() {
    let mut leader = elected_leader(A, vec![A, B, C]);
    assert!(leader.begin_membership_change(vec![B, C]));
    let term = leader.term();
    let reply = |follower, match_index| AppendReply {
      read_context: 0,
      follower,
      term,
      success: true,
      match_index,
      conflict_term: 0,
      conflict_index: 0,
      priority: ElectionPriority::default(),
    };
    // The joint entry (index 1): B with A carries the old set but not the new one, so the change cannot
    // complete yet; C's acknowledgement commits it, and the joint configuration still names the leader.
    leader.on_append_reply(reply(B, 1));
    let completed_early = leader.complete_membership_change();
    leader.on_append_reply(reply(C, 1));
    let joint = (completed_early, leader.commit_index(), leader.is_leader());
    assert_eq!(
      joint,
      (false, 1, true),
      "no completion before the joint entry commits; committed by C; still leading while joint"
    );
    let completed = leader.complete_membership_change();
    assert_eq!(
      (completed, leader.all_voters()),
      (true, vec![B, C]),
      "C_new is appended once the joint entry committed"
    );
    // C_new (index 2) commits under the new majority alone (B and C): the leader steps down, is no longer
    // a voter, and never campaigns again.
    leader.on_append_reply(reply(B, 2));
    leader.on_append_reply(reply(C, 2));
    let campaign = leader.on_election_timeout();
    assert_eq!(
      (
        leader.commit_index(),
        leader.role(),
        leader.is_voter(A),
        campaign.is_empty()
      ),
      (2, Role::Follower, false, true),
      "committed under the new majority; the removed leader stepped down and does not campaign"
    );
  }

  /// One change at a time (thesis §4.1): a change cannot begin while the previous configuration entry is
  /// uncommitted, a joint change cannot complete before its joint entry commits, and an empty voter set is
  /// never a target.
  #[test]
  fn a_change_waits_for_the_previous_configuration_entry_to_commit() {
    let mut leader = elected_leader(A, vec![A, B, C]);
    assert!(
      !leader.begin_membership_change(Vec::new()),
      "an empty voter set is refused"
    );
    assert!(leader.begin_membership_change(vec![A, B]));
    assert!(
      !leader.complete_membership_change(),
      "the joint entry is not yet committed"
    );
    let term = leader.term();
    let reply = |follower, match_index| AppendReply {
      read_context: 0,
      follower,
      term,
      success: true,
      match_index,
      conflict_term: 0,
      conflict_index: 0,
      priority: ElectionPriority::default(),
    };
    leader.on_append_reply(reply(B, 1));
    assert_eq!(
      leader.commit_index(),
      1,
      "A and B carry both the old and the new set"
    );
    assert!(leader.complete_membership_change());
    assert!(
      !leader.begin_membership_change(vec![A]),
      "C_new is uncommitted: no further change may begin"
    );
    leader.on_append_reply(reply(B, 2));
    assert_eq!(leader.commit_index(), 2);
    assert!(
      leader.begin_membership_change(vec![A]),
      "once it commits the next change may begin"
    );
  }

  /// Outgoing voters stay replication targets until the entry that removes them commits — so a live
  /// member demoted to learner receives it — and drop out the moment it does.
  #[test]
  fn outgoing_voters_are_replicated_to_until_their_removal_commits() {
    let mut leader = elected_leader(A, vec![A, B, C]);
    assert!(leader.begin_membership_change(vec![A, B]));
    let term = leader.term();
    let reply = |follower, match_index| AppendReply {
      read_context: 0,
      follower,
      term,
      success: true,
      match_index,
      conflict_term: 0,
      conflict_index: 0,
      priority: ElectionPriority::default(),
    };
    leader.on_append_reply(reply(B, 1));
    assert!(leader.complete_membership_change());
    assert_eq!(
      leader.all_voters(),
      vec![A, B],
      "C_new is in effect on append"
    );
    assert_eq!(
      leader.replication_targets(),
      vec![A, B, C],
      "C, outgoing, is still replicated to while C_new is uncommitted"
    );
    leader.on_append_reply(reply(B, 2));
    assert_eq!(leader.commit_index(), 2);
    assert_eq!(
      leader.replication_targets(),
      vec![A, B],
      "committed: the outgoing voter drops out"
    );
  }

  /// Compaction (Raft §7) folds the committed prefix into a snapshot and discards it, bounding the log,
  /// while every index still resolves — the last index is unchanged and appends continue past the
  /// snapshot. Compacting backward or beyond the commit index is refused.
  #[test]
  fn compaction_bounds_the_log_while_indices_stay_correct() {
    // A lone leader commits every append at once, so five appends give a five-entry committed log.
    let mut leader = elected_leader(A, vec![A]);
    for value in 0..5u8 {
      leader.append_command(vec![value]);
    }
    assert_eq!(leader.commit_index(), 5);
    assert_eq!(leader.last_log_index(), 5);

    // Compact up to index 3: the prefix is discarded, but the last index and the committed remainder are
    // still correct.
    assert!(
      leader.compact(3, Vec::new()),
      "committed entries up to 3 compact"
    );
    assert_eq!(leader.snapshot_index(), 3);
    assert_eq!(
      leader.last_log_index(),
      5,
      "the last index is unchanged by compaction"
    );
    assert_eq!(
      leader.committed_entries().len(),
      2,
      "only the entries above the snapshot (indices 4 and 5) remain to apply"
    );

    // Appends continue past the snapshot boundary and still commit.
    leader.append_command(b"after-snapshot".to_vec());
    assert_eq!(leader.last_log_index(), 6);
    assert_eq!(
      leader.commit_index(),
      6,
      "the entry after the snapshot commits"
    );
  }

  /// Compaction is refused backward (at or below the current snapshot) and ahead of the commit index —
  /// only the committed, not-yet-snapshotted prefix may be discarded.
  #[test]
  fn compaction_refuses_backward_or_uncommitted() {
    let mut leader = elected_leader(A, vec![A]);
    for value in 0..3u8 {
      leader.append_command(vec![value]);
    }
    assert!(
      leader.compact(2, Vec::new()),
      "committed entries up to 2 compact"
    );
    assert!(
      !leader.compact(2, Vec::new()),
      "cannot compact at or below the current snapshot"
    );
    assert!(!leader.compact(1, Vec::new()), "nor backward");
    assert!(
      !leader.compact(100, Vec::new()),
      "nor beyond the commit index"
    );
  }

  /// After compaction the leader still replicates correctly to a follower: the append it builds anchors
  /// at the snapshot boundary (using the snapshot term), and a follower that already holds that prefix
  /// accepts the entries beyond it.
  #[test]
  fn replication_works_across_a_snapshot_boundary() {
    // A three-node leader with a committed three-entry log (recovered so the log exists), elected fresh.
    let mut leader = node_with_uncommitted_log(A, vec![A, B, C], 2, None, log_of(&[1, 1, 2]));
    leader.start_election(); // term 3
    leader.on_vote_reply(VoteReply {
      voter: B,
      term: leader.term(),
      granted: true,
      reports: Vec::new(),
    });
    assert!(leader.is_leader());
    leader.append_command(b"t3".to_vec()); // index 4, term 3

    // Commit index 4 by replicating to a follower that holds the term-1/term-2 prefix.
    let mut follower = node_with_uncommitted_log(B, vec![A, B, C], 3, None, log_of(&[1, 1, 2]));
    let append = leader.replicate_to(B, UNBOUNDED).expect("append");
    let reply = follower.on_append_entries(append);
    assert!(reply.success);
    leader.on_append_reply(reply);
    assert_eq!(leader.commit_index(), 4);

    // Compact the leader up to index 2; its log now begins after the snapshot.
    assert!(leader.compact(2, Vec::new()));
    assert_eq!(leader.snapshot_index(), 2);

    // Append and replicate again: the follower (already caught up) accepts across the boundary.
    leader.append_command(b"t3-more".to_vec()); // index 5
    let append = leader
      .replicate_to(B, UNBOUNDED)
      .expect("append after compaction");
    assert!(
      append.prev_log_index >= leader.snapshot_index(),
      "the append anchors at or after the snapshot"
    );
    let reply = follower.on_append_entries(append);
    assert!(
      reply.success,
      "the caught-up follower accepts the post-compaction append"
    );
    assert_eq!(follower.last_log_index(), 5);
  }

  /// A follower fallen below the leader's snapshot is caught up by an install-snapshot (Raft §7):
  /// replication first backs its next index down to the snapshot boundary and then cannot proceed (the
  /// entries are compacted away), so the leader ships the snapshot; the follower installs it and then
  /// accepts the entries beyond it.
  #[test]
  fn a_follower_below_the_snapshot_is_caught_up_by_install_snapshot() {
    // A term-3 leader with a four-entry committed log, compacted up to index 3 with some snapshot state.
    let mut leader = node_with_uncommitted_log(A, vec![A, B, C], 2, None, log_of(&[1, 1, 2]));
    leader.start_election(); // term 3
    leader.on_vote_reply(VoteReply {
      voter: B,
      term: leader.term(),
      granted: true,
      reports: Vec::new(),
    });
    leader.append_command(b"t3".to_vec()); // index 4, term 3
    let mut follower_b = node_with_uncommitted_log(B, vec![A, B, C], 3, None, log_of(&[1, 1, 2]));
    let append = leader.replicate_to(B, UNBOUNDED).expect("append");
    let reply = follower_b.on_append_entries(append);
    leader.on_append_reply(reply);
    assert_eq!(leader.commit_index(), 4);
    assert!(leader.compact(3, b"snapshot-state".to_vec()));

    // A fresh, empty follower C is far below the snapshot. Replication backs its next index down until an
    // append can no longer be built (the previous entry is compacted away).
    let mut follower_c = RaftNode::new(C, vec![A, B, C]);
    let needs_snapshot = replicate_until_stuck(&mut leader, &mut follower_c, C);
    assert!(
      needs_snapshot,
      "replication cannot reach a follower below the snapshot"
    );

    // The leader ships the snapshot; the follower installs it and reports back.
    let snapshot = leader.install_snapshot_for(C).expect("C needs a snapshot");
    assert_eq!(snapshot.last_included_index, 3);
    assert_eq!(snapshot.state, b"snapshot-state");
    let reply = follower_c.on_install_snapshot(snapshot);
    leader.on_install_snapshot_reply(reply);
    assert_eq!(
      follower_c.snapshot_index(),
      3,
      "the follower adopted the snapshot"
    );

    // Now a normal append carries the entries beyond the snapshot, and C is caught up.
    let append = leader
      .replicate_to(C, UNBOUNDED)
      .expect("append after the snapshot");
    let reply = follower_c.on_append_entries(append);
    assert!(reply.success, "C accepts the post-snapshot entries");
    assert_eq!(
      follower_c.last_log_index(),
      4,
      "C is caught up to the leader"
    );
  }

  /// The log-integrated membership change (Raft §6): a configuration entry takes effect the moment it is
  /// appended — the node is joint before the entry commits — and reverts when the entry is truncated,
  /// because the effective configuration is derived from the log, not stored.
  #[test]
  fn a_configuration_takes_effect_on_append_and_reverts_on_truncation() {
    let mut leader = elected_leader(A, vec![A, B, C]);
    assert!(!leader.in_joint_configuration());
    leader.begin_membership_change(vec![C, D, E]);
    assert!(
      leader.in_joint_configuration(),
      "the joint configuration takes effect on append, before it commits"
    );

    // A follower adopts the joint configuration when it receives the entry.
    let mut follower = RaftNode::new(B, vec![A, B, C]);
    let append = leader
      .replicate_to(B, UNBOUNDED)
      .expect("append carrying the configuration entry");
    follower.on_append_entries(append);
    assert!(
      follower.in_joint_configuration(),
      "the follower adopts it on append"
    );

    // A conflicting entry at index 1 from a newer term truncates the configuration entry, reverting the
    // configuration to the base.
    let conflicting = AppendEntries {
      read_context: 0,
      term: follower.term() + 1,
      leader: A,
      prev_log_index: 0,
      prev_log_term: 0,
      entries: vec![LogEntry::command(follower.term() + 1, b"other".to_vec())],
      leader_commit: 0,
      priorities: Vec::new(),
      sync_index: 0,
      open_from: 0,
    };
    let reply = follower.on_append_entries(conflicting);
    assert!(reply.success);
    assert!(
      !follower.in_joint_configuration(),
      "truncating the configuration entry reverts the configuration"
    );
  }

  /// Compaction preserves the effective configuration: a configuration entry folded into the snapshot is
  /// carried into the base, so the node stays in the joint configuration after its log is compacted.
  #[test]
  fn compaction_preserves_the_configuration() {
    let mut leader = elected_leader(A, vec![A, B, C]);
    leader.begin_membership_change(vec![B, C, D]); // joint {A,B,C} ∪ {B,C,D} at index 1
    assert!(leader.in_joint_configuration());

    // Replicate the joint entry to B and C — a majority of both configurations — so it commits.
    for id in [B, C] {
      let mut follower = RaftNode::new(id, vec![A, B, C]);
      let append = leader.replicate_to(id, UNBOUNDED).expect("append");
      let reply = follower.on_append_entries(append);
      leader.on_append_reply(reply);
    }
    assert_eq!(
      leader.commit_index(),
      1,
      "the joint configuration entry commits"
    );

    // Compact past it; the joint configuration survives in the base.
    assert!(leader.compact(1, b"state".to_vec()));
    assert!(
      leader.in_joint_configuration(),
      "the configuration folded into the snapshot is preserved"
    );
  }

  /// Raft §7 with §5.3 (`docs/bugs/2026-09-28-a-late-append-could-land-compacted-entries-on-a-log.md`): a
  /// late copy of an append anchored below a follower's compacted prefix leaves its log exactly as it was —
  /// the follower matches the leader through its commit index, since every committed entry is the same on
  /// every server — and the reply says so. Before, the append anchored at zero pushed the entries the
  /// snapshot already holds onto the end of the log.
  #[test]
  fn a_late_append_below_a_compacted_prefix_leaves_the_log_whole() {
    let mut leader = elected_leader(A, vec![A, B, C]);
    for value in 0..5u8 {
      leader.append_command(vec![value]);
    }
    let mut follower = RaftNode::new(B, vec![A, B, C]);
    let append = leader.replicate_to(B, UNBOUNDED).expect("an append");
    let late_copy = append.clone();
    let reply = follower.on_append_entries(append);
    leader.on_append_reply(reply);
    let heartbeat = leader.replicate_to(B, UNBOUNDED).expect("a heartbeat");
    follower.on_append_entries(heartbeat);
    assert_eq!(follower.commit_index(), 5);
    assert!(follower.compact(3, b"state".to_vec()));
    let before = follower.saved();
    let reply = follower.on_append_entries(late_copy);
    assert!(reply.success);
    assert_eq!(
      reply.match_index, 5,
      "it matches the leader through its commit index"
    );
    assert_eq!(follower.saved(), before, "the log is exactly as it was");
  }

  /// Thesis §4.2.1 and Raft §5.3: a follower whose log does not reach the leader's previous index says
  /// where its log ends, so the leader backs up to it in one round trip — an empty follower behind twenty
  /// entries costs one refusal, not twenty.
  #[test]
  fn an_empty_follower_is_found_in_one_refusal() {
    let mut leader = node_with_uncommitted_log(A, vec![A, B, C], 1, None, log_of(&[1; 20]));
    leader.start_election();
    leader.on_vote_reply(VoteReply {
      voter: C,
      term: leader.term(),
      granted: true,
      reports: Vec::new(),
    });
    assert!(leader.is_leader());
    let mut follower = RaftNode::new(B, vec![A, B, C]);
    assert_eq!(catch_up(&mut leader, &mut follower, B), 1);
    assert_eq!(follower.last_log_index(), 20);
  }

  /// Raft §5.3: a follower holding a run of a stale term's entries names the term and where the run
  /// begins, so the leader skips the whole run in one round trip — eight diverged entries cost one refusal,
  /// not eight — and the follower's log ends as the leader's.
  #[test]
  fn a_stale_terms_run_is_skipped_in_one_refusal() {
    let mut leader = node_with_uncommitted_log(
      A,
      vec![A, B, C],
      3,
      None,
      log_of(&[1, 1, 3, 3, 3, 3, 3, 3, 3, 3]),
    );
    leader.start_election();
    leader.on_vote_reply(VoteReply {
      voter: C,
      term: leader.term(),
      granted: true,
      reports: Vec::new(),
    });
    assert!(leader.is_leader());
    let mut follower = node_with_uncommitted_log(
      B,
      vec![A, B, C],
      2,
      None,
      log_of(&[1, 1, 2, 2, 2, 2, 2, 2, 2, 2]),
    );
    assert_eq!(catch_up(&mut leader, &mut follower, B), 1);
    assert_eq!(follower.saved().log, leader.saved().log);
  }

  /// Replicates from `leader` to `follower` (id `who`) until an append succeeds, returning the refusals it
  /// took; bounded by the leader's log.
  fn catch_up(leader: &mut RaftNode, follower: &mut RaftNode, who: HostId) -> u64 {
    let mut refusals = 0;
    loop {
      let append = leader.replicate_to(who, UNBOUNDED).expect("an append");
      let reply = follower.on_append_entries(append);
      leader.on_append_reply(reply);
      if reply.success {
        return refusals;
      }
      refusals += 1;
      assert!(refusals <= leader.last_log_index(), "bounded by the log");
    }
  }

  /// An append carries at most its budget of entry bytes ([`LogEntry::encoded_len`]), and never nothing
  /// while an entry is owed: a budget smaller than one entry still sends that entry, alone, so replication
  /// always progresses.
  #[test]
  fn an_append_carries_its_budget_and_never_nothing_when_owed() {
    let mut leader = elected_leader(A, vec![A, B, C]);
    for value in 0..10u8 {
      leader.append_command(vec![value]);
    }
    let entry = LogEntry::command(leader.term(), vec![0]).encoded_len();
    let mut batch = |budget: usize| {
      leader
        .replicate_to(B, budget)
        .expect("an append")
        .entries
        .len()
    };
    assert_eq!(batch(2 * entry), 2);
    assert_eq!(batch(3 * entry - 1), 2);
    for budget in [0, 1, entry - 1] {
      assert_eq!(batch(budget), 1, "budget {budget}");
    }
    assert_eq!(batch(UNBOUNDED), 10);
  }

  /// Thesis §3.5 (a follower's matched prefix only grows within a term): a late reply never moves its
  /// progress back — after the follower matched through ten, an earlier success through four and an earlier
  /// refusal hinting at the log's start leave the next append anchored at ten.
  #[test]
  fn late_replies_never_move_progress_back() {
    let mut leader = elected_leader(A, vec![A, B, C]);
    for value in 0..10u8 {
      leader.append_command(vec![value]);
    }
    let mut follower = RaftNode::new(B, vec![A, B, C]);
    let reply = follower.on_append_entries(leader.replicate_to(B, UNBOUNDED).expect("an append"));
    leader.on_append_reply(reply);
    assert_eq!(reply.match_index, 10);
    leader.on_append_reply(AppendReply {
      match_index: 4,
      ..reply
    });
    leader.on_append_reply(AppendReply {
      success: false,
      match_index: 0,
      conflict_term: 0,
      conflict_index: 1,
      ..reply
    });
    let next = leader.replicate_to(B, UNBOUNDED).expect("a heartbeat");
    assert_eq!(next.prev_log_index, 10);
  }

  /// A leader with four entries committed with B and compacted through three, and an empty C that needs
  /// its snapshot.
  fn leader_owing_c_a_snapshot() -> (RaftNode, RaftNode) {
    let mut leader = elected_leader(A, vec![A, B, C]);
    for value in 0..4u8 {
      leader.append_command(vec![value]);
    }
    let mut follower_b = RaftNode::new(B, vec![A, B, C]);
    let reply = follower_b.on_append_entries(leader.replicate_to(B, UNBOUNDED).expect("an append"));
    leader.on_append_reply(reply);
    assert_eq!(leader.commit_index(), 4);
    assert!(leader.compact(3, b"state-3".to_vec()));
    let follower_c = RaftNode::new(C, vec![A, B, C]);
    (leader, follower_c)
  }

  /// A follower that declines a snapshot (its state did not decode, `crate::fold`) is credited with
  /// nothing: the leader owes it the snapshot still, and the follower adopted none of it.
  #[test]
  fn a_declined_snapshot_credits_the_follower_with_nothing() {
    let (mut leader, mut follower_c) = leader_owing_c_a_snapshot();
    let _ = catch_up_or_stuck(&mut leader, &mut follower_c);
    let snapshot = leader
      .install_snapshot_for(C)
      .expect("C needs the snapshot");
    let reply = follower_c.decline_snapshot(&snapshot);
    assert_eq!(reply.match_index, 0);
    leader.on_install_snapshot_reply(reply);
    assert!(leader.install_snapshot_for(C).is_some(), "C still needs it");
    assert_eq!(follower_c.snapshot_index(), 0);
  }

  /// The leader credits a snapshot's recipient with what the recipient says it holds, never with its own
  /// snapshot index when the reply arrives: a leader that compacted further while the snapshot was in
  /// flight still owes the follower the newer one.
  #[test]
  fn a_snapshot_reply_credits_what_the_follower_holds() {
    let (mut leader, mut follower_c) = leader_owing_c_a_snapshot();
    let _ = catch_up_or_stuck(&mut leader, &mut follower_c);
    let snapshot = leader
      .install_snapshot_for(C)
      .expect("C needs the snapshot");
    let reply = follower_c.on_install_snapshot(snapshot);
    assert_eq!(reply.match_index, 3);
    assert!(leader.compact(4, b"state-4".to_vec()));
    leader.on_install_snapshot_reply(reply);
    let again = leader
      .install_snapshot_for(C)
      .expect("C holds only through 3, below the leader's snapshot at 4");
    assert_eq!(again.last_included_index, 4);
  }

  /// Replicates from `leader` to C until an append can no longer be built (C needs a snapshot) or C
  /// accepts; returns whether it got stuck. Bounded by the leader's log.
  fn catch_up_or_stuck(leader: &mut RaftNode, follower_c: &mut RaftNode) -> bool {
    for _ in 0..=leader.last_log_index() {
      let Some(append) = leader.replicate_to(C, UNBOUNDED) else {
        return true;
      };
      let reply = follower_c.on_append_entries(append);
      leader.on_append_reply(reply);
      if reply.success {
        return false;
      }
    }
    false
  }

  /// A leader of {A, B, C} holding ten committed entries (B took them), and D, a fresh node outside the
  /// voter set.
  fn leader_with_history() -> (RaftNode, RaftNode, RaftNode) {
    let mut leader = elected_leader(A, vec![A, B, C]);
    for value in 0..10u8 {
      leader.append_command(vec![value]);
    }
    let mut follower_b = RaftNode::new(B, vec![A, B, C]);
    let reply = follower_b.on_append_entries(leader.replicate_to(B, UNBOUNDED).expect("an append"));
    leader.on_append_reply(reply);
    assert_eq!(leader.commit_index(), 10);
    let member = RaftNode::new(D, vec![A, B, C]);
    (leader, follower_b, member)
  }

  /// One replication round from `leader` to the staged `member` (id D).
  fn replicate_to_d(leader: &mut RaftNode, member: &mut RaftNode) {
    if let Some(append) = leader.replicate_to(D, UNBOUNDED) {
      let reply = member.on_append_entries(append);
      leader.on_append_reply(reply);
    }
  }

  /// Thesis §4.2.1: a member a voter set adds is caught up before the change may begin — the leader stages it,
  /// replicates to it (it is a replication target, never a voter), and reports it ready once a round has
  /// completed within one CheckQuorum window; its acknowledgements count toward no commit.
  #[test]
  fn a_new_member_is_caught_up_before_the_change_may_begin() {
    let (mut leader, _follower_b, mut member) = leader_with_history();
    let target = [A, B, C, D];
    assert_eq!(leader.catch_up(&target), CatchUp::Pending);
    assert!(
      leader.replication_targets().contains(&D),
      "D is replicated to"
    );
    assert!(!leader.is_voter(D), "but it does not vote");
    for _ in 0..3 {
      replicate_to_d(&mut leader, &mut member);
    }
    assert_eq!(member.last_log_index(), 10, "D holds the log");
    assert_eq!(leader.catch_up(&target), CatchUp::Ready);
    assert!(leader.begin_membership_change(target.to_vec()));
  }

  /// Learners count toward nothing (thesis §4.2.1: "not yet counted towards majorities"): with B and C
  /// silent, a staged D taking every entry commits none of them.
  #[test]
  fn a_staged_member_counts_toward_no_commit() {
    let (mut leader, _follower_b, mut member) = leader_with_history();
    assert_eq!(leader.catch_up(&[A, B, C, D]), CatchUp::Pending);
    leader.append_command(b"new".to_vec());
    for _ in 0..3 {
      replicate_to_d(&mut leader, &mut member);
    }
    assert_eq!(member.last_log_index(), 11, "D took the new entry");
    assert_eq!(
      leader.commit_index(),
      10,
      "D's acknowledgement committed nothing"
    );
  }

  /// Thesis §4.2.1's abort: a staged member that never answers has its staging ended at the second
  /// CheckQuorum tick — the baseline, then one whole election timeout without progress — reported once and
  /// counted; the next call stages it afresh ("the caller may always try again"). One tick is not enough.
  #[test]
  fn a_member_that_never_answers_is_aborted_and_staged_afresh_after() {
    let (mut leader, _follower_b, _member) = leader_with_history();
    let target = [A, B, C, D];
    assert_eq!(leader.catch_up(&target), CatchUp::Pending);
    for tick in 0..=STALLED_WINDOWS {
      if tick == STALLED_WINDOWS {
        assert_eq!(
          leader.catch_up(&target),
          CatchUp::Pending,
          "the baseline tick alone aborts nothing"
        );
      }
      // B and C keep the leader in contact, so CheckQuorum does not depose it.
      for voter in [B, C] {
        leader.on_append_reply(AppendReply {
          read_context: 0,
          follower: voter,
          term: leader.term(),
          success: true,
          match_index: 0,
          conflict_term: 0,
          conflict_index: 0,
          priority: ElectionPriority::default(),
        });
      }
      leader.check_quorum();
      assert!(leader.is_leader(), "tick {tick}");
    }
    assert_eq!(leader.catch_up(&target), CatchUp::Aborted { member: D });
    assert_eq!(leader.stagings_aborted(), 1);
    assert_eq!(leader.catch_up(&target), CatchUp::Pending, "staged afresh");
  }

  /// A round that spans a CheckQuorum window does not count (it may have lasted an election timeout); the
  /// next round, completing within a window, does.
  #[test]
  fn a_round_that_spans_a_window_is_followed_by_one_that_counts() {
    let (mut leader, _follower_b, mut member) = leader_with_history();
    let target = [A, B, C, D];
    assert_eq!(leader.catch_up(&target), CatchUp::Pending);
    for voter in [B, C] {
      leader.on_append_reply(AppendReply {
        read_context: 0,
        follower: voter,
        term: leader.term(),
        success: true,
        match_index: 0,
        conflict_term: 0,
        conflict_index: 0,
        priority: ElectionPriority::default(),
      });
    }
    leader.check_quorum();
    for _ in 0..3 {
      replicate_to_d(&mut leader, &mut member);
    }
    assert_eq!(member.last_log_index(), 10);
    assert_eq!(
      leader.staged(),
      vec![(D, true)],
      "the first round spanned a window; the next completed within one"
    );
    assert_eq!(leader.catch_up(&target), CatchUp::Ready);
  }

  /// Staging is leader-local: a leader that loses leadership forgets it, and a node that leads again starts
  /// with none.
  #[test]
  fn staging_ends_with_leadership() {
    let (mut leader, _follower_b, _member) = leader_with_history();
    assert_eq!(leader.catch_up(&[A, B, C, D]), CatchUp::Pending);
    leader.observe_term(leader.term() + 1);
    assert!(!leader.is_leader());
    assert!(leader.staged().is_empty());
    assert!(!leader.replication_targets().contains(&D));
    assert_eq!(leader.catch_up(&[A, B, C, D]), CatchUp::NotLeader);
  }

  /// Shape: the entry bytes one append carries in the availability-gap test — about two of its entries — so
  /// a lagging member takes many rounds to catch up, as it takes many heartbeats on a real session.
  const GAP_BUDGET: usize = 32;
  /// Shape: the entries the group holds before the change — a lag of about twenty rounds at the gap budget.
  const GAP_HISTORY: u8 = 40;

  /// One replication round from `leader` to each of `alive` it replicates to, each reply folded.
  fn gap_round(leader: &mut RaftNode, alive: &mut BTreeMap<HostId, RaftNode>) {
    for target in leader.replication_targets() {
      let Some(follower) = alive.get_mut(&target) else {
        continue;
      };
      if let Some(append) = leader.replicate_to(target, GAP_BUDGET) {
        let reply = follower.on_append_entries(append);
        leader.on_append_reply(reply);
      }
    }
  }

  /// Thesis §4.2.1, Figure 4.4(a), replayed: voters {A, B, C} hold [`GAP_HISTORY`] entries; D joins with an
  /// empty log and the voters become {A, B, C, D}; then C fails, so a commit needs three of four and D must
  /// hold it. Returns the replication rounds the first entry proposed after C's failure took to commit.
  fn rounds_to_commit_after_a_loss(staged: bool) -> u64 {
    let mut leader = elected_leader(A, vec![A, B, C]);
    let mut alive: BTreeMap<HostId, RaftNode> = [B, C]
      .into_iter()
      .map(|id| (id, RaftNode::new(id, vec![A, B, C])))
      .collect();
    for value in 0..GAP_HISTORY {
      leader.append_command(vec![value]);
    }
    while leader.commit_index() < u64::from(GAP_HISTORY) {
      gap_round(&mut leader, &mut alive);
    }
    alive.insert(D, RaftNode::new(D, vec![A, B, C]));
    let target = [A, B, C, D];
    if staged {
      while leader.catch_up(&target) != CatchUp::Ready {
        gap_round(&mut leader, &mut alive);
      }
    }
    assert!(leader.begin_membership_change(target.to_vec()));
    while leader.in_joint_configuration() {
      gap_round(&mut leader, &mut alive);
      leader.complete_membership_change();
    }
    while leader.commit_index() < leader.last_log_index() {
      gap_round(&mut leader, &mut alive);
    }
    alive.remove(&C);
    leader.append_command(b"after the loss".to_vec());
    let proposed = leader.last_log_index();
    let mut rounds = 0;
    while leader.commit_index() < proposed {
      gap_round(&mut leader, &mut alive);
      rounds += 1;
      assert!(rounds <= u64::from(GAP_HISTORY), "bounded");
    }
    rounds
  }

  /// Thesis §4.2.1 ("if a fourth server with an empty log is added ... and one of the original three servers
  /// fails, the cluster will be temporarily unable to commit new entries"): added directly, the newcomer
  /// leaves the group unable to commit for as many rounds as it takes to catch up; staged first, the group
  /// commits in the first round after the loss.
  #[test]
  fn a_staged_newcomer_leaves_no_availability_gap_where_a_direct_one_does() {
    let direct = rounds_to_commit_after_a_loss(false);
    let staged = rounds_to_commit_after_a_loss(true);
    eprintln!("rounds to the first commit after the loss: direct {direct}, staged {staged}");
    assert_eq!(staged, 1, "the staged newcomer held the log already");
    assert!(
      direct > staged + 5,
      "the direct newcomer's catch-up held commits back: {direct} rounds"
    );
  }

  /// A priority of `quorum_ms` ± `spread_ms`, in nanoseconds.
  fn priority_ms(quorum_ms: u64, spread_ms: u64) -> ElectionPriority {
    ElectionPriority {
      quorum_ns: quorum_ms * 1_000_000,
      spread_ns: spread_ms * 1_000_000,
    }
  }

  /// §3.4: a priority outranks another only when its interval — round trip plus spread — lies wholly below
  /// the other's round trip less its spread; overlapping intervals tie both ways, and an unknown priority
  /// (zero) neither outranks nor is outranked.
  #[test]
  fn only_a_wholly_lower_interval_outranks() {
    let central = priority_ms(117, 10);
    let edge = priority_ms(169, 10);
    let near_edge = priority_ms(125, 10);
    assert!(central.outranks(&edge));
    assert!(!edge.outranks(&central));
    assert!(
      !central.outranks(&near_edge) && !near_edge.outranks(&central),
      "overlapping intervals tie"
    );
    let unknown = ElectionPriority::default();
    assert!(!unknown.outranks(&edge) && !central.outranks(&unknown));
  }

  /// A follower of {A, B, C} that took the leader's priority table from its append.
  fn follower_with_table(own: ElectionPriority, table: &[(HostId, ElectionPriority)]) -> RaftNode {
    let mut follower = RaftNode::new(C, vec![A, B, C]);
    follower.set_priority(own);
    let reply = follower.on_append_entries(AppendEntries {
      read_context: 0,
      term: 1,
      leader: A,
      prev_log_index: 0,
      prev_log_term: 0,
      entries: Vec::new(),
      leader_commit: 0,
      priorities: table.to_vec(),
      sync_index: 0,
      open_from: 0,
    });
    assert!(reply.success);
    assert_eq!(reply.priority, own, "the reply carries its own priority");
    follower
  }

  /// §3.4's rank: a follower counts the live voters of the leader's table that outrank it — not the dead,
  /// not the tied, not the unknown.
  #[test]
  fn a_follower_ranks_against_the_leaders_table_among_live_voters() {
    let follower = follower_with_table(
      priority_ms(169, 10),
      &[(A, priority_ms(117, 10)), (B, priority_ms(162, 10))],
    );
    assert_eq!(
      follower.election_rank(&[A, B]),
      1,
      "A outranks C; B ties it"
    );
    assert_eq!(follower.election_rank(&[B]), 0, "a dead A outranks no one");
    let unmeasured = follower_with_table(
      ElectionPriority::default(),
      &[(A, priority_ms(117, 10)), (B, priority_ms(162, 10))],
    );
    assert_eq!(unmeasured.election_rank(&[A, B]), 0, "unknown ranks first");
  }

  /// B and C answer the leader this window, each carrying its priority (B 117 ms, C 140 ms, ± 10 ms), so
  /// CheckQuorum keeps it leading and its table holds both.
  fn b_and_c_answer(leader: &mut RaftNode) {
    for (voter, quorum) in [(B, 117), (C, 140)] {
      leader.on_append_reply(AppendReply {
        read_context: 0,
        follower: voter,
        term: leader.term(),
        success: true,
        match_index: 0,
        conflict_term: 0,
        conflict_index: 0,
        priority: priority_ms(quorum, 10),
      });
    }
  }

  /// §3.4's transfer: a leader hands off to the live voter that outranks it most, only after two
  /// CheckQuorum ticks of leading — never to a dead one, never when nothing outranks it — and after a
  /// priority transfer aborts, not again during this leadership.
  #[test]
  fn a_leader_hands_off_by_priority_after_a_whole_window_and_once_after_an_abort() {
    let mut leader = elected_leader(A, vec![A, B, C]);
    leader.set_priority(priority_ms(169, 10));
    b_and_c_answer(&mut leader);
    assert_eq!(
      leader.priority_transfer(&[B, C]),
      None,
      "not before a whole window"
    );
    for _ in 0..PRIORITY_WINDOWS {
      b_and_c_answer(&mut leader);
      leader.check_quorum();
    }
    assert!(leader.is_leader());
    assert_eq!(
      leader.priority_transfer(&[C]),
      Some(C),
      "B is dead: C, which also outranks A"
    );
    // The transfer to C never completes: two ticks abort it, and the latch holds for this leadership.
    for _ in 0..TRANSFER_QUORUM_CHECKS {
      b_and_c_answer(&mut leader);
      leader.check_quorum();
    }
    assert!(leader.transferring_to().is_none(), "aborted");
    assert_eq!(
      leader.priority_transfer(&[B, C]),
      None,
      "no second try this leadership"
    );
    assert_eq!(leader.priority_transfers(), 1);
  }

  /// Shape: a window budget with room for several small commands (a one-byte command's entry is 14 bytes on
  /// the wire), so the tests of every rule but the bound stay clear of it.
  const WINDOW_BUDGET: usize = 256;

  /// Replicates `leader`'s log to each follower once and folds the replies.
  fn replicate_once(leader: &mut RaftNode, followers: &mut [(HostId, &mut RaftNode)]) {
    for (who, follower) in followers.iter_mut() {
      let append = leader
        .replicate_to(*who, UNBOUNDED)
        .expect("a leader replicates");
      let reply = follower.on_append_entries(append);
      leader.on_append_reply(reply);
    }
  }

  /// A group of `voters` with windows: the first leads, its sync point (the no-op a group appends on winning)
  /// has reached every other voter, which is synced to its term, and its fast track is open and announced.
  fn fast_group(voters: &[HostId]) -> Vec<RaftNode> {
    let mut nodes: Vec<RaftNode> = voters
      .iter()
      .map(|id| {
        if *id == voters[0] {
          elected_leader(*id, voters.to_vec())
        } else {
          RaftNode::new(*id, voters.to_vec())
        }
      })
      .collect();
    for node in &mut nodes {
      node.set_window_budget(WINDOW_BUDGET);
    }
    let (leader, followers) = nodes.split_first_mut().unwrap();
    assert!(leader.append_command(Vec::new()), "the sync point");
    let mut reached: Vec<(HostId, &mut RaftNode)> =
      followers.iter_mut().map(|node| (node.id(), node)).collect();
    replicate_once(leader, &mut reached);
    assert!(leader.open_fast_track());
    replicate_once(leader, &mut reached);
    nodes
  }

  /// §3.7: a proposal every voter votes commits in one round from the proposer — the leader decides it at the
  /// first classic quorum of votes and commits it at the fast quorum, before any follower's log holds it. The
  /// commit is the leader's to apply and acknowledge; its commit index, which followers learn and windows prune
  /// under, stays classic (§4).
  #[test]
  fn a_proposal_a_fast_quorum_votes_commits_in_one_round() {
    let mut nodes = fast_group(&[A, B, C]);
    let proposal = nodes[1].propose_fast(b"x".to_vec()).unwrap();
    assert_eq!(proposal.index, 2, "the first index after the sync point");
    let votes: Vec<FastVote> = nodes
      .iter_mut()
      .filter_map(|node| node.on_fast_propose(proposal.clone()))
      .collect();
    assert_eq!(votes.len(), 3);
    let leader = &mut nodes[0];
    for vote in votes {
      leader.on_fast_vote(vote);
    }
    assert_eq!((leader.committed_through(), leader.commit_index()), (2, 1));
    assert_eq!(leader.committed_entries().last().unwrap().command, b"x");
    let counters = leader.window_counters();
    assert_eq!((counters.decided_from_votes, counters.fast_commits), (1, 1));
  }

  /// §3.7: two proposals at one index split the votes. The leader decides the one a classic quorum carries,
  /// and — no fast quorum voting it — it commits once a majority's logs hold it, as a classic entry; the
  /// losing vote stays in its voter's window until the commit, then goes.
  #[test]
  fn split_votes_are_decided_by_the_leader_and_commit_through_its_log() {
    let mut nodes = fast_group(&[A, B, C]);
    let x = nodes[1].propose_fast(b"x".to_vec()).unwrap();
    let y = nodes[2].propose_fast(b"y".to_vec()).unwrap();
    assert_eq!(
      (x.index, y.index),
      (2, 2),
      "both proposers think index 2 unused"
    );
    let votes = [
      nodes[0].on_fast_propose(x.clone()).unwrap(),
      nodes[1].on_fast_propose(x).unwrap(),
      nodes[2].on_fast_propose(y).unwrap(),
    ];
    assert_eq!(votes[2].command, b"y");
    for vote in votes {
      nodes[0].on_fast_vote(vote);
    }
    let (leader, followers) = nodes.split_first_mut().unwrap();
    assert_eq!(leader.last_log_index(), 2, "decided");
    assert_eq!(leader.commit_index(), 1, "no fast quorum voted it");
    let mut reached: Vec<(HostId, &mut RaftNode)> =
      followers.iter_mut().map(|node| (node.id(), node)).collect();
    replicate_once(leader, &mut reached);
    assert_eq!(leader.commit_index(), 2);
    assert_eq!(leader.committed_entries().last().unwrap().command, b"x");
    assert_eq!(leader.window_counters().fast_commits, 0);
    replicate_once(leader, &mut reached);
    assert!(
      nodes[2].window().is_empty(),
      "the committed index's slot is gone"
    );
  }

  /// §4, the recovery: every voter votes a proposal, and the leader stops before it hears a vote. A new leader
  /// elected by two of them finds the value in their windows — two reports, at least `2 + 3 − 3` — and
  /// re-proposes it at the same index: a fast quorum may have chosen it.
  #[test]
  fn a_new_leader_recovers_a_fast_choice_from_its_voters_windows() {
    let mut nodes = fast_group(&[A, B, C]);
    let proposal = nodes[1].propose_fast(b"x".to_vec()).unwrap();
    for node in nodes.iter_mut() {
      node.on_fast_propose(proposal.clone()).unwrap();
    }
    let request = nodes[1].start_election().into_iter().next().unwrap();
    let reply = nodes[2].on_request_vote(request);
    assert!(reply.granted);
    assert_eq!(reply.reports.len(), 1, "C reports its vote at index 2");
    nodes[1].on_vote_reply(reply);
    let successor = &nodes[1];
    assert!(successor.is_leader());
    let entry = &successor.saved().log[1];
    assert_eq!(
      (entry.term, entry.command.as_slice()),
      (successor.term(), b"x".as_slice())
    );
    let counters = successor.window_counters();
    assert_eq!(
      (counters.recovered, counters.recovered_fast_choices),
      (1, 1)
    );
  }

  /// A five-voter group where `voted` voters voted `x` at index 2 and the leader stopped: `C` is then elected
  /// by `C`, `D` and `E`, and reports whether it re-proposed `x`.
  fn recovers_with_votes(voted: &[usize]) -> bool {
    let mut nodes = fast_group(&[A, B, C, D, E]);
    let proposal = nodes[1].propose_fast(b"x".to_vec()).unwrap();
    for at in voted {
      nodes[*at].on_fast_propose(proposal.clone()).unwrap();
    }
    let requests = nodes[2].start_election();
    for (at, request) in [(3, requests[2]), (4, requests[3])] {
      let reply = nodes[at].on_request_vote(request);
      nodes[2].on_vote_reply(reply);
    }
    assert!(nodes[2].is_leader());
    nodes[2]
      .saved()
      .log
      .get(1)
      .is_some_and(|entry| entry.command == b"x")
  }

  /// §4, Fast Paxos's threshold, five voters (a fast quorum of four): with three reports heard, a value two of
  /// them carry could have been chosen (with the two unheard, four), and is re-proposed; a value one carries
  /// could not (three at most), and its index is left free.
  #[test]
  fn a_value_short_of_the_threshold_leaves_its_index_free() {
    assert!(
      recovers_with_votes(&[2, 3]),
      "two reports of three reach 3 + 4 − 5"
    );
    assert!(!recovers_with_votes(&[2]), "one report is short of it");
  }

  /// §4, syncing: a voter keeps its fast vote of an older term when its log reaches the next leader's sync
  /// point, and drops it once a classic commit covers its index — until then a later leader's truncation could
  /// erase the synced entries, and with them the only record of the vote (the prefix model's 18 steps).
  #[test]
  fn a_synced_follower_keeps_older_slots_until_a_classic_commit_covers_them() {
    let mut nodes = fast_group(&[A, B, C]);
    let proposal = nodes[1].propose_fast(b"x".to_vec()).unwrap();
    nodes[2].on_fast_propose(proposal).unwrap();
    assert_eq!(nodes[2].window().len(), 1);
    let requests = nodes[1].start_election();
    let reply = nodes[0].on_request_vote(requests[0]);
    nodes[1].on_vote_reply(reply);
    assert!(nodes[1].is_leader());
    assert!(nodes[1].append_command(Vec::new()), "the new sync point");
    let (successor, rest) = nodes.split_at_mut(2);
    replicate_once(&mut successor[1], &mut [(C, &mut rest[0])]);
    assert_eq!(
      rest[0].window().len(),
      1,
      "C is synced and keeps its vote of term 1"
    );
    replicate_once(&mut successor[1], &mut [(C, &mut rest[0])]);
    assert!(
      rest[0].window().is_empty(),
      "the classic commit of index 2 covers it"
    );
    assert_eq!(rest[0].window_counters().pruned, 1);
  }

  /// §3.5, out-of-order acknowledgement: a synced follower keeps the leader's entry that arrives ahead of a hole
  /// in its window, and absorbs it into its log the moment the hole fills — no second send of it.
  #[test]
  fn a_follower_buffers_the_leaders_entries_ahead_of_a_hole_and_absorbs_them() {
    let mut leader = elected_leader(A, vec![A, B, C]);
    let mut follower = RaftNode::new(B, vec![A, B, C]);
    leader.set_window_budget(WINDOW_BUDGET);
    follower.set_window_budget(WINDOW_BUDGET);
    assert!(leader.append_command(Vec::new()), "the sync point");
    replicate_once(&mut leader, &mut [(B, &mut follower)]);
    assert!(leader.append_command(b"x".to_vec()));
    assert!(leader.append_command(b"y".to_vec()));
    let both = leader.replicate_to(B, UNBOUNDED).unwrap();
    assert_eq!(both.entries.len(), 2);
    let ahead = AppendEntries {
      prev_log_index: 2,
      prev_log_term: both.entries[0].term,
      entries: vec![both.entries[1].clone()],
      ..both.clone()
    };
    let behind = AppendEntries {
      entries: vec![both.entries[0].clone()],
      ..both
    };
    let refused = follower.on_append_entries(ahead);
    assert!(!refused.success, "the hole at index 2");
    assert_eq!(follower.window_counters().buffered, 1);
    let accepted = follower.on_append_entries(behind);
    assert_eq!(accepted.match_index, 3, "the buffered entry joined the log");
    assert_eq!(follower.window_counters().absorbed, 1);
    assert_eq!(follower.last_log_index(), 3);
  }

  /// §4: while this term's fast track is open, a classic append and a membership change are refused — a classic
  /// entry at an open index could contradict a fast choice, and fast votes count against one configuration.
  #[test]
  fn the_fast_track_refuses_classic_appends_and_membership_changes() {
    let mut nodes = fast_group(&[A, B, C]);
    assert!(!nodes[0].append_command(b"x".to_vec()));
    assert!(!nodes[0].begin_membership_change(vec![A, B]));
  }

  /// §3.7, liveness under loss: a proposal that reached too few voters leaves its index short of a classic
  /// quorum of votes, and the leader decides in order, so every later index waits behind it — the proposer's
  /// resend goes to a new index and never fills it. The leader fills the hole itself (Fast Paxos's
  /// coordinator-run round): it proposes a no-op there on its own fast track, a voter that voted re-sends its
  /// vote and one that did not votes the no-op, and the classic quorum it then hears decides by the ballot rule.
  /// Until 2026-09-29 nothing filled it: at 4 % loss across five regions a proposer lost up to half its
  /// commands.
  #[test]
  fn a_leader_fills_a_hole_its_votes_left() {
    let mut nodes = fast_group(&[A, B, C]);
    let short = nodes[1].propose_fast(b"x".to_vec()).unwrap();
    let vote = nodes[1].on_fast_propose(short).unwrap();
    nodes[0].on_fast_vote(vote);
    let whole = nodes[1].propose_fast(b"y".to_vec()).unwrap();
    assert_eq!(whole.index, 3);
    let votes: Vec<FastVote> = nodes
      .iter_mut()
      .filter_map(|node| node.on_fast_propose(whole.clone()))
      .collect();
    for vote in votes {
      nodes[0].on_fast_vote(vote);
    }
    assert_eq!(
      nodes[0].last_log_index(),
      1,
      "index 2 has one vote of the two a decision needs"
    );
    assert_eq!(nodes[0].stalled_index(), Some(2));
    let fill = nodes[0].fill_hole(2).unwrap();
    assert_eq!((fill.index, fill.command.as_slice()), (2, b"".as_slice()));
    let votes: Vec<FastVote> = nodes
      .iter_mut()
      .filter_map(|node| node.on_fast_propose(fill.clone()))
      .collect();
    for vote in votes {
      nodes[0].on_fast_vote(vote);
    }
    assert_eq!(
      nodes[0].last_log_index(),
      3,
      "both decided: the hole, then y behind it"
    );
    assert_eq!(nodes[0].stalled_index(), None);
    // The no-op carried two votes of three, so it commits classically, and y's fast commit with it.
    let (leader, followers) = nodes.split_first_mut().unwrap();
    let mut reached: Vec<(HostId, &mut RaftNode)> =
      followers.iter_mut().map(|node| (node.id(), node)).collect();
    replicate_once(leader, &mut reached);
    let commands: Vec<Vec<u8>> = leader
      .committed_entries()
      .iter()
      .map(|entry| entry.command.clone())
      .collect();
    assert_eq!(
      commands,
      vec![Vec::new(), Vec::new(), b"y".to_vec()],
      "the no-op in the hole, then y"
    );
  }

  /// §4, the bounds: a voter votes only within its window's span above its log, and only while the window's
  /// budget has room; zero budget, the default, holds nothing and opens no fast track.
  #[test]
  fn the_window_is_bounded_by_its_budget_and_span() {
    let mut nodes = fast_group(&[A, B, C]);
    let far = FastPropose {
      term: nodes[1].term(),
      proposer: B,
      index: nodes[1].last_log_index()
        + u64::try_from(WINDOW_BUDGET / MIN_ENTRY_BYTES).unwrap()
        + 1,
      command: b"x".to_vec(),
    };
    assert!(nodes[1].on_fast_propose(far).is_none(), "beyond the span");
    let large = nodes[1].propose_fast(vec![0; WINDOW_BUDGET]).unwrap();
    assert!(
      nodes[1].on_fast_propose(large).is_none(),
      "beyond the budget"
    );
    let mut unbudgeted = elected_leader(A, vec![A]);
    assert!(unbudgeted.append_command(Vec::new()));
    assert!(!unbudgeted.open_fast_track(), "no window, no fast track");
  }

  /// §4.8 retention: a window is retained as the log is — every slot an accepted value — and a publication
  /// whose window is damaged is refused before the node votes.
  #[test]
  fn a_retained_window_survives_restore_and_a_damaged_one_is_refused() {
    use slates_wire::Wire;
    let mut nodes = fast_group(&[A, B, C]);
    let proposal = nodes[1].propose_fast(b"x".to_vec()).unwrap();
    nodes[1].on_fast_propose(proposal).unwrap();
    let saved = nodes[1].saved();
    assert_eq!(saved.window.len(), 1);
    let mut bytes = Vec::new();
    saved.encode(&mut bytes);
    let decoded = SavedRaft::decode(&mut bytes.as_slice()).unwrap();
    let restored = RaftNode::restore(decoded).unwrap();
    assert_eq!(restored.window(), nodes[1].window());
    for damage in [
      |saved: &mut SavedRaft| saved.window[0].index = 0,
      |saved: &mut SavedRaft| saved.window[0].slot.term = saved.term + 1,
      |saved: &mut SavedRaft| saved.synced_term = saved.term + 1,
    ] {
      let mut damaged = nodes[1].saved();
      damage(&mut damaged);
      assert_eq!(
        RaftNode::restore(damaged).err(),
        Some(RaftRecoveryError::InvalidWindow)
      );
    }
  }

  /// §4, the bounds over a term's life: the leader votes on the fast track too, into its own window, and its
  /// slots go under a classic commit as a follower's do — so a window of three entries carries ten fast
  /// commits. Until 2026-09-29 only a follower pruned: the leader's window filled after three, it stopped
  /// voting, and a three-voter group (a fast quorum of three) committed nothing more on the fast track.
  #[test]
  fn a_leaders_own_votes_leave_its_window_at_commit() {
    let mut nodes = fast_group(&[A, B, C]);
    let three_entries = 3 * (MIN_ENTRY_BYTES + 1);
    for node in &mut nodes {
      node.set_window_budget(three_entries);
    }
    for round in 0..10u8 {
      let proposal = nodes[1].propose_fast(vec![round]).unwrap();
      let votes: Vec<FastVote> = nodes
        .iter_mut()
        .filter_map(|node| node.on_fast_propose(proposal.clone()))
        .collect();
      for vote in votes {
        nodes[0].on_fast_vote(vote);
      }
      let (leader, followers) = nodes.split_first_mut().unwrap();
      let mut reached: Vec<(HostId, &mut RaftNode)> =
        followers.iter_mut().map(|node| (node.id(), node)).collect();
      replicate_once(leader, &mut reached);
    }
    assert_eq!(nodes[0].window_counters().fast_commits, 10);
    assert!(
      nodes[0].window().is_empty(),
      "every slot of the leader's is committed"
    );
  }

  /// Thesis §3.10 with the fast track open: a leader handing off stops deciding from votes, as it stops
  /// accepting proposals, so the target's log can match its own; the votes stay in the voters' windows for the
  /// successor's recovery.
  #[test]
  fn a_transferring_leader_decides_nothing_from_votes() {
    let mut nodes = fast_group(&[A, B, C]);
    nodes[0].transfer_leadership(B).unwrap();
    assert!(
      nodes[0].propose_fast(b"own".to_vec()).is_none(),
      "the leader proposes nothing"
    );
    let proposal = nodes[1].propose_fast(b"x".to_vec()).unwrap();
    let before = nodes[0].last_log_index();
    for at in [1, 2] {
      let vote = nodes[at].on_fast_propose(proposal.clone()).unwrap();
      nodes[0].on_fast_vote(vote);
    }
    assert_eq!(
      nodes[0].last_log_index(),
      before,
      "a classic quorum voted, and nothing was decided"
    );
    assert_eq!(
      nodes[2].window().len(),
      1,
      "the vote waits for the successor"
    );
    for _ in 0..TRANSFER_QUORUM_CHECKS {
      nodes[0].check_quorum();
    }
    assert_eq!(nodes[0].transfers_aborted(), 1);
    assert_eq!(
      nodes[0].last_log_index(),
      before + 1,
      "the waiting votes are decided at the abort"
    );
  }

  /// §3.7: an opening of the fast track counts in the term it was announced in only. `B` heard term 1's opening,
  /// then won term 2 by its own election and never opened the track, then timed out, leaving leadership in the
  /// same term. Until 2026-09-29 the term-1 announcement still stood, so `B` proposed and voted on a fast track
  /// no leader of term 2 had opened (the explorer's first run with the fast track, seed 0 at three voters).
  #[test]
  fn an_opening_counts_in_its_own_term_only() {
    let mut nodes = fast_group(&[A, B, C]);
    assert!(nodes[1].fast_track_open(), "B heard term 1's opening");
    let requests = nodes[1].start_election();
    let reply = nodes[2].on_request_vote(requests[1]);
    nodes[1].on_vote_reply(reply);
    assert!(nodes[1].is_leader());
    assert!(
      !nodes[1].fast_track_open(),
      "B leads term 2 and opened nothing"
    );
    nodes[1].on_election_timeout();
    assert!(!nodes[1].is_leader());
    assert!(
      !nodes[1].fast_track_open(),
      "no leader of term 2 opened the track"
    );
    assert!(nodes[1].propose_fast(b"x".to_vec()).is_none());
  }

  /// Three voters where the leader `A` fast-committed `x` at index 2 and `y` at 3 and then went quiet, and `B`
  /// won the next term with `C`'s reports and re-proposed both at its own term, then appended its sync point
  /// (§4's recovery). `A` holds the two commands under term 1, `B` the same two under term 2. Returns `A`, `B`
  /// and `C`.
  fn a_fast_commit_re_proposed_by_a_successor() -> (RaftNode, RaftNode, RaftNode) {
    let mut nodes = fast_group(&[A, B, C]);
    for command in [b"x", b"y"] {
      let proposal = nodes[1].propose_fast(command.to_vec()).unwrap();
      let votes: Vec<FastVote> = nodes
        .iter_mut()
        .filter_map(|node| node.on_fast_propose(proposal.clone()))
        .collect();
      for vote in votes {
        nodes[0].on_fast_vote(vote);
      }
    }
    assert_eq!(
      (nodes[0].committed_through(), nodes[0].commit_index()),
      (3, 1),
      "A fast-committed both; its commit index stays classic"
    );
    let [mut a, mut b, mut c]: [RaftNode; 3] = nodes.try_into().ok().unwrap();
    let requests = b.start_election();
    b.on_vote_reply(c.on_request_vote(requests[1]));
    assert!(b.is_leader());
    assert!(b.append_command(Vec::new()), "B's sync point");
    let terms =
      |node: &RaftNode| -> Vec<u64> { node.saved().log.iter().map(|entry| entry.term).collect() };
    assert_eq!((terms(&a), terms(&b)), (vec![1, 1, 1], vec![1, 2, 2, 2]));
    let _ = &mut a;
    (a, b, c)
  }

  /// §4: a fast commit is the committing leader's alone — to apply and acknowledge — so when a successor
  /// re-proposes the commands under its own term, the old leader takes the successor's entries as any follower
  /// takes a leader's: its log ends up the successor's, entry for entry and term for term. Committed entries
  /// therefore never differ in term between nodes (the prefix model never takes the path that would keep one
  /// under an older term).
  #[test]
  fn an_old_leaders_fast_commits_yield_to_its_successors_entries() {
    let (mut a, mut b, mut c) = a_fast_commit_re_proposed_by_a_successor();
    // Rounds, not one: a follower's first refusal backs the leader up before it takes the entries.
    for _ in 0..3 {
      replicate_once(&mut b, &mut [(A, &mut a), (C, &mut c)]);
    }
    assert_eq!(a.saved().log, b.saved().log);
    assert_eq!((b.commit_index(), a.commit_index()), (4, 4));
    let commands: Vec<Vec<u8>> = a
      .committed_entries()
      .iter()
      .map(|entry| entry.command.clone())
      .collect();
    assert_eq!(
      commands,
      vec![Vec::new(), b"x".to_vec(), b"y".to_vec(), Vec::new()]
    );
  }

  /// §4: a new leader keeps its window — its recovery reads it, but a slot goes only once a classic commit
  /// covers its index. Clearing it at the election lost a chosen value in the explorer (seed 266): the new
  /// leader's log carried the value until a later leader's truncation erased it, and then nothing did.
  #[test]
  fn a_new_leader_keeps_its_window_until_a_classic_commit() {
    let mut nodes = fast_group(&[A, B, C]);
    let proposal = nodes[1].propose_fast(b"x".to_vec()).unwrap();
    for at in [1, 2] {
      nodes[at].on_fast_propose(proposal.clone()).unwrap();
    }
    let requests = nodes[1].start_election();
    let reply = nodes[2].on_request_vote(requests[1]);
    nodes[1].on_vote_reply(reply);
    assert!(nodes[1].is_leader());
    assert_eq!(nodes[1].window_counters().recovered_fast_choices, 1);
    assert_eq!(nodes[1].window().len(), 1, "B's own vote stays");
    assert!(nodes[1].append_command(Vec::new()), "the sync point");
    let (_, rest) = nodes.split_at_mut(1);
    let (b, c) = rest.split_at_mut(1);
    for _ in 0..2 {
      replicate_once(&mut b[0], &mut [(C, &mut c[0])]);
    }
    assert!(
      b[0].window().is_empty(),
      "the classic commit covers index 2"
    );
  }

  /// §4, one configuration per fast term: the fast track opens only on a committed configuration. A leader
  /// that has appended `C_new` must wait for it to commit — a successor lacking it would take the joint
  /// configuration as its own and treat an index a fast quorum chose under `C_new` as free.
  #[test]
  fn the_fast_track_waits_for_the_configuration_to_commit() {
    let mut leader = elected_leader(A, vec![A, B, C]);
    let mut b = RaftNode::new(B, vec![A, B, C]);
    for node in [&mut leader, &mut b] {
      node.set_window_budget(WINDOW_BUDGET);
    }
    assert!(leader.append_command(Vec::new()), "the sync point");
    replicate_once(&mut leader, &mut [(B, &mut b)]);
    assert!(leader.begin_membership_change(vec![A, B]));
    replicate_once(&mut leader, &mut [(B, &mut b)]);
    assert!(leader.complete_membership_change());
    assert!(!leader.in_joint_configuration());
    assert!(
      !leader.open_fast_track(),
      "C_new is appended, not committed"
    );
    replicate_once(&mut leader, &mut [(B, &mut b)]);
    assert_eq!(leader.commit_index(), leader.last_log_index());
    assert!(leader.open_fast_track(), "C_new committed");
  }

  /// Shape: a batch budget of one one-byte command's entry, so each append carries one entry and every batch
  /// boundary is visible.
  const ONE_ENTRY: usize = MIN_ENTRY_BYTES + 1;

  /// A leader `A` whose sync point `B` and `C` hold (their places confirmed), every node's window `window`
  /// bytes, and `commands` one-byte commands appended after it.
  fn pipelining_group(window: usize, commands: u8) -> Vec<RaftNode> {
    let mut nodes: Vec<RaftNode> = [A, B, C]
      .iter()
      .map(|id| {
        let mut node = if *id == A {
          elected_leader(A, vec![A, B, C])
        } else {
          RaftNode::new(*id, vec![A, B, C])
        };
        node.set_window_budget(window);
        node
      })
      .collect();
    let (leader, followers) = nodes.split_first_mut().unwrap();
    assert!(leader.append_command(Vec::new()), "the sync point");
    let mut reached: Vec<(HostId, &mut RaftNode)> =
      followers.iter_mut().map(|node| (node.id(), node)).collect();
    replicate_once(leader, &mut reached);
    for command in 0..commands {
      assert!(leader.append_command(vec![command]));
    }
    nodes
  }

  /// §3.5 with thesis §10.2.2: a follower whose place is confirmed is sent the next batch before the last is
  /// acknowledged — as many batches beyond the first unacknowledged one as its window holds — and then, with
  /// no room, the first unacknowledged batch again. Until 2026-09-29 every send restarted at the first
  /// unacknowledged entry, so a follower never had an entry ahead of a hole to buffer.
  #[test]
  fn a_confirmed_follower_is_sent_batches_ahead_within_its_window() {
    let mut nodes = pipelining_group(3 * ONE_ENTRY, 5);
    let leader = &mut nodes[0];
    let sent: Vec<u64> = (0..5)
      .map(|_| leader.replicate_to(B, ONE_ENTRY).unwrap().prev_log_index)
      .collect();
    assert_eq!(
      sent,
      vec![1, 2, 3, 4, 1],
      "the first batch, three ahead, then the first again"
    );
  }

  /// §3.5: a batch lost ahead of buffered ones costs one resend of the hole — the follower buffered the later
  /// batches, refused them over the hole, and acknowledges them all when the hole fills — and they are never
  /// sent again.
  #[test]
  fn a_lost_batch_costs_one_resend_and_the_buffered_ones_are_not_sent_again() {
    let mut nodes = pipelining_group(3 * ONE_ENTRY, 3);
    let (leader, followers) = nodes.split_first_mut().unwrap();
    let follower = &mut followers[0];
    let appends: Vec<AppendEntries> = (0..3)
      .map(|_| leader.replicate_to(B, ONE_ENTRY).unwrap())
      .collect();
    for append in appends.into_iter().skip(1) {
      let reply = follower.on_append_entries(append);
      assert!(!reply.success, "the hole at index 2");
      leader.on_append_reply(reply);
    }
    assert_eq!(follower.window_counters().buffered, 2);
    let resend = leader.replicate_to(B, ONE_ENTRY).unwrap();
    assert_eq!(
      (resend.prev_log_index, resend.entries.len()),
      (1, 1),
      "the hole's batch alone"
    );
    let reply = follower.on_append_entries(resend);
    assert_eq!(reply.match_index, 4, "the buffered batches joined the log");
    leader.on_append_reply(reply);
    assert_eq!(follower.window_counters().absorbed, 2);
    let after = leader.replicate_to(B, ONE_ENTRY).unwrap();
    assert_eq!(
      (after.prev_log_index, after.entries.len()),
      (4, 0),
      "a heartbeat: nothing is owed"
    );
  }

  /// §4, the recovery reads every report above its log, as the prefix model's does, not only its own window's
  /// reach: a node with a smaller window than its voters' must still recover what they chose beyond it. `B`
  /// reaches one index past its log and cannot vote at index 3; the other four — a fast quorum of five — vote
  /// `y` there, so it is chosen. Until 2026-09-29 `B`, elected by `C` and `D`, read reports within its own
  /// reach only and left index 3 free, where its next entry would have replaced a chosen command.
  #[test]
  fn a_recovery_reads_every_report_beyond_its_own_reach() {
    let mut nodes = fast_group(&[A, B, C, D, E]);
    nodes[1].set_window_budget(MIN_ENTRY_BYTES + 1);
    for command in [b"x", b"y"] {
      let proposal = nodes[0].propose_fast(command.to_vec()).unwrap();
      for node in nodes.iter_mut() {
        node.on_fast_propose(proposal.clone());
      }
    }
    assert_eq!(nodes[1].window().len(), 1, "B voted at index 2 only");
    let requests = nodes[1].start_election();
    for (at, request) in [(2, requests[1]), (3, requests[2])] {
      let reply = nodes[at].on_request_vote(request);
      nodes[1].on_vote_reply(reply);
    }
    assert!(nodes[1].is_leader());
    let commands: Vec<Vec<u8>> = nodes[1]
      .saved()
      .log
      .iter()
      .map(|entry| entry.command.clone())
      .collect();
    assert_eq!(commands, vec![Vec::new(), b"x".to_vec(), b"y".to_vec()]);
    assert_eq!(nodes[1].window_counters().recovered_beyond_reach, 1);
  }

  /// §4, a recovery from a decision: `C` buffered the leader's entry that arrived ahead of a lost one, and the
  /// leader stopped. `B`, elected by `C`, finds that slot's ballot — a decision, which outranks any fast vote —
  /// and re-proposes it as it is, at its own term, with a no-op in the hole below it.
  #[test]
  fn a_new_leader_re_proposes_a_buffered_decision() {
    let mut nodes = pipelining_group(3 * ONE_ENTRY, 2);
    let lost = nodes[0].replicate_to(C, ONE_ENTRY).unwrap();
    let ahead = nodes[0].replicate_to(C, ONE_ENTRY).unwrap();
    assert_eq!((lost.prev_log_index, ahead.prev_log_index), (1, 2));
    assert!(
      !nodes[2].on_append_entries(ahead).success,
      "the hole at index 2"
    );
    assert_eq!(nodes[2].window_counters().buffered, 1);
    let requests = nodes[1].start_election();
    let reply = nodes[2].on_request_vote(requests[1]);
    nodes[1].on_vote_reply(reply);
    assert!(nodes[1].is_leader());
    let log = nodes[1].saved().log;
    let term = nodes[1].term();
    assert_eq!(
      log
        .iter()
        .map(|entry| (entry.term, entry.command.clone()))
        .collect::<Vec<_>>(),
      vec![(1, Vec::new()), (term, Vec::new()), (term, vec![1])],
      "the sync point, a no-op in the hole, the decision re-proposed"
    );
    let counters = nodes[1].window_counters();
    assert_eq!(
      (
        counters.recovered,
        counters.recovered_fast_choices,
        counters.holes_filled
      ),
      (1, 0, 1)
    );
  }

  /// §3.5: when a resend of the first unacknowledged batch would carry the new entries anyway — the backlog
  /// fits one batch, the groups' usual case — the leader resends rather than sending ahead, whatever the
  /// window, so a lost batch is recovered by the next send and not a round trip later.
  #[test]
  fn a_backlog_within_one_batch_is_resent_not_sent_ahead() {
    let mut nodes = pipelining_group(3 * ONE_ENTRY, 1);
    let leader = &mut nodes[0];
    let first = leader.replicate_to(B, 3 * ONE_ENTRY).unwrap();
    assert_eq!((first.prev_log_index, first.entries.len()), (1, 1));
    for command in [7, 8] {
      assert!(leader.append_command(vec![command]));
    }
    let second = leader.replicate_to(B, 3 * ONE_ENTRY).unwrap();
    assert_eq!(
      (second.prev_log_index, second.entries.len()),
      (1, 3),
      "the unacknowledged entry and the two new ones, in one resend"
    );
  }

  /// §3.5: a backlog past what one resend carries goes ahead even when the unacknowledged batch was not full.
  /// Until 2026-09-29 the leader resent whenever a resend reached past what it had sent — every period, at a
  /// steady rate just short of a batch a period — and never sent ahead however far its backlog grew (the
  /// timed simulation's five regions at 2,000 proposals a second: no batch sent ahead, 1,018 commits a
  /// second).
  #[test]
  fn a_backlog_past_one_resend_goes_ahead_after_a_short_batch() {
    let mut nodes = pipelining_group(3 * ONE_ENTRY, 1);
    let leader = &mut nodes[0];
    let short = leader.replicate_to(B, 3 * ONE_ENTRY).unwrap();
    assert_eq!((short.prev_log_index, short.entries.len()), (1, 1));
    for command in 10..15 {
      assert!(leader.append_command(vec![command]));
    }
    let next = leader.replicate_to(B, 3 * ONE_ENTRY).unwrap();
    assert_eq!(
      (next.prev_log_index, next.entries.len()),
      (2, 3),
      "five new entries past a one-entry batch: a resend of three would not reach the log's end"
    );
  }

  /// With no window — the groups' default — nothing is sent ahead: every send starts at the first
  /// unacknowledged entry, exactly as before pipelining.
  #[test]
  fn with_no_window_nothing_is_sent_ahead() {
    let mut nodes = pipelining_group(0, 5);
    let leader = &mut nodes[0];
    let sent: Vec<u64> = (0..3)
      .map(|_| leader.replicate_to(B, ONE_ENTRY).unwrap().prev_log_index)
      .collect();
    assert_eq!(sent, vec![1, 1, 1]);
  }

  /// The recovery reads its window by filter, not by `BTreeMap::range`, which panics on a start past its end:
  /// a node holding a slot with a zero budget — its reach is its log's end — once aborted here on winning an
  /// election. It wins now, recovering nothing beyond its reach.
  #[test]
  fn a_node_holding_a_window_with_no_budget_wins_without_panicking() {
    let mut nodes = fast_group(&[A, B, C]);
    let proposal = nodes[1].propose_fast(b"x".to_vec()).unwrap();
    nodes[1].on_fast_propose(proposal).unwrap();
    nodes[1].set_window_budget(0);
    let requests = nodes[1].start_election();
    let reply = nodes[2].on_request_vote(requests[1]);
    nodes[1].on_vote_reply(reply);
    assert!(nodes[1].is_leader());
    assert_eq!(nodes[1].window_counters().recovered, 0);
  }
}
