//! A **timed** simulation of the configuration groups' Raft (§4.8 mechanism 2;
//! `docs/wip/research/consensus-enhancements.md` §5): a deterministic, seeded, discrete-event model of a
//! group of real [`RaftNode`]s on a virtual clock, driven the way the council's drive loop drives them
//! (`slates_server::fleet::drive_config_council`): one period per heartbeat per node, the leader replicating to
//! the other voters, sending a leadership transfer's invitation, and judging its quorum on the CheckQuorum
//! cadence ([`ElectionTimer::leader_period`]); a follower ageing the real [`ElectionTimer`] against the
//! leader-contact count (a current leader's append and a granted vote reset it) and campaigning through the
//! pre-vote round; a new leader appending its current-term no-op. The election timing is derived from the
//! measured round trips to the other voters exactly as the daemon derives it ([`ElectionTiming::derive`]).
//!
//! The network is modelled per directed pair: a one-way latency, uniform jitter on top, and random loss; a
//! partition is a window during which messages between its two sides are lost; a crash is a window during
//! which a node neither ticks nor receives, after which it restarts from what it retained. A steady
//! proposal stream at the leader measures what users see: the latency from a proposal to its commit, and
//! the longest a proposal waited — the unavailability a fault costs.
//!
//! The protocol-level counterpart of the explorer (`tests/explore.rs`, which checks safety over orders with
//! no clock): this one measures time. Everything is bounded by the scenario's duration.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

use slates_cluster::raft::{ElectionPriority, RaftNode, TimeoutNow};
use slates_cluster::raft_wire::{RaftMessage, append_batch_bytes};
use slates_cluster::timing::{
  ElectionTimer, ElectionTiming, PathRtt, REPAIR_ROUND_TRIPS, quorum_priority,
};
use slates_db::register::HostId;
use slates_transport::endpoint::MAX_PACKET_PAYLOAD;

/// Format: the group envelope the council's messages ride in (`slates_server::consensus`): a 32-byte group
/// id and the message's 4-byte length.
const ENVELOPE_BYTES: usize = 32 + 4;
/// Shape: the periods of silence after which a node stops holding a peer alive — the daemon's two-period
/// suspicion and one for the probe interval.
const ALIVE_PERIODS: u64 = 3;

/// Shape: the heartbeat period every node ticks at — the daemon's `HEARTBEAT_NS` (100 ms), mirrored so a
/// period here is a period there.
pub(crate) const HEARTBEAT_NS: u64 = 100_000_000;
/// Format: a millisecond in nanoseconds.
pub(crate) const MS: u64 = 1_000_000;

/// A network profile: every directed pair's one-way latency, jitter and loss, with per-pair overrides (a
/// multi-region matrix is a profile whose pairs across regions override the in-region default).
#[derive(Clone, Debug)]
pub(crate) struct Profile {
  /// One-way latency of a pair with no override.
  pub(crate) one_way_ns: u64,
  /// Uniform jitter added to each message's latency, `[0, jitter_ns)`.
  pub(crate) jitter_ns: u64,
  /// Probability a message is lost, parts per million.
  pub(crate) loss_ppm: u32,
  /// `(from, to) → (one_way_ns, jitter_ns)` overrides.
  pub(crate) pairs: BTreeMap<(HostId, HostId), (u64, u64)>,
}

impl Profile {
  /// A uniform profile.
  pub(crate) fn uniform(one_way_ns: u64, jitter_ns: u64, loss_ppm: u32) -> Profile {
    Profile {
      one_way_ns,
      jitter_ns,
      loss_ppm,
      pairs: BTreeMap::new(),
    }
  }

  fn latency(&self, from: HostId, to: HostId) -> (u64, u64) {
    self
      .pairs
      .get(&(from, to))
      .copied()
      .unwrap_or((self.one_way_ns, self.jitter_ns))
  }
}

/// A fault in force over `[from_ns, until_ns)`.
#[derive(Clone, Debug)]
pub(crate) enum Fault {
  /// The nodes in `isolated` can reach each other but nobody outside, and nobody outside reaches them.
  Partition {
    /// One side of the cut.
    isolated: BTreeSet<HostId>,
    /// When the cut begins.
    from_ns: u64,
    /// When it heals.
    until_ns: u64,
  },
  /// Whichever node follows the leader at `from_ns` (the lowest-id follower) is cut off until `until_ns`.
  IsolateFollower {
    /// When the cut begins (the follower is chosen then).
    from_ns: u64,
    /// When it heals.
    until_ns: u64,
  },
  /// Whichever node leads at `from_ns` is cut off until `until_ns`.
  IsolateLeader {
    /// When the cut begins (the leader is chosen then).
    from_ns: u64,
    /// When it heals.
    until_ns: u64,
  },
  /// The node is down: it neither ticks nor receives; at `until_ns` it restarts from what it retained.
  Crash {
    /// The node.
    node: HostId,
    /// When it crashes.
    from_ns: u64,
    /// When it restarts.
    until_ns: u64,
  },
}

/// How a follower campaigns when its timer fires — the experiment's variable. `PreVote` is the dialect (the
/// council's `election_timeout`); `Direct` skips the pre-vote round (`start_election`) and exists only here,
/// as the control a measurement of pre-vote compares against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Campaign {
  /// The pre-vote round first (Raft §9.6).
  PreVote,
  /// A real election at once — the control.
  Direct,
}

/// How elections are ordered (`docs/wip/research/consensus-enhancements.md` §3.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ElectionOrder {
  /// The daemon's drive: each node measures its election priority, the timer yields to live voters that
  /// outrank it, and a leader hands off to one that does.
  ByPriority,
  /// The control: every priority left unknown, so the first timeout wins — the drive before §3.4. A harness
  /// variant, never a production path.
  ByTimeout,
}

/// A scenario: the voters, the network, the faults, how long it runs, and the proposal stream.
#[derive(Clone, Debug)]
pub(crate) struct Scenario {
  /// The voters `HostId(1)..=HostId(voters)`.
  pub(crate) voters: u64,
  /// The network.
  pub(crate) profile: Profile,
  /// The faults.
  pub(crate) faults: Vec<Fault>,
  /// Virtual time the scenario runs for.
  pub(crate) duration_ns: u64,
  /// The leader proposes one command every this many nanoseconds (none when zero).
  pub(crate) propose_every_ns: u64,
  /// When the proposal stream begins — after the first election has settled, so the startup election is
  /// not counted as unavailability.
  pub(crate) propose_from_ns: u64,
  /// How followers campaign.
  pub(crate) campaign: Campaign,
  /// How elections are ordered.
  pub(crate) order: ElectionOrder,
  /// The seed for jitter, loss and tick phases.
  pub(crate) seed: u64,
  /// How every node's window is set (`RaftNode::set_window_budget`): what a follower can hold ahead of a hole,
  /// and so how far a leader sends ahead of acknowledgements.
  pub(crate) window: Window,
  /// Who proposes the stream.
  pub(crate) proposer: Proposer,
  /// Whether a new leader opens its term's fast track after its no-op (research record §3.7).
  pub(crate) fast_track: bool,
}

/// Who proposes the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Proposer {
  /// The leader, whoever it is: each proposal is appended at the leader's tick, and a new leader's stream
  /// restarts; its latency runs to the leader's commit.
  Leader,
  /// A fixed node, as a client beside it would: each proposal goes to its leader (classic) or, once the fast
  /// track is open to it, to every voter (§3.7), and is sent again when not committed within
  /// [`REPAIR_ROUND_TRIPS`] round trips of its slowest voter path; its latency runs to when that node learns
  /// the commit — the leader's commit and the leader's one-way path to it.
  At(HostId),
}

/// How every node's window is set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Window {
  /// A fixed number of wire bytes on every node; zero holds none and sends nothing ahead.
  Bytes(usize),
  /// Each node's own, set every period from its measured voter paths as the daemon sets it
  /// (`ElectionTiming::window_budget` over the council drive's batch budget).
  Derived,
}

/// What a scenario measured.
#[derive(Clone, Debug, Default)]
pub(crate) struct Outcome {
  /// Every time a node became leader: (virtual time, node, term).
  pub(crate) leader_events: Vec<(u64, HostId, u64)>,
  /// Campaigns begun (a follower's timer firing, or an invitation).
  pub(crate) campaigns: u64,
  /// Every campaign: (virtual time, node).
  pub(crate) campaign_events: Vec<(u64, HostId)>,
  /// Each committed proposal's latency, proposal to the leader's commit.
  pub(crate) commit_latencies_ns: Vec<u64>,
  /// The longest time the proposal stream went without a commit (the unavailability).
  pub(crate) longest_gap_ns: u64,
  /// The highest term any node reached.
  pub(crate) max_term: u64,
  /// Each node's term at the end.
  pub(crate) final_terms: BTreeMap<HostId, u64>,
  /// Messages sent.
  pub(crate) messages: u64,
  /// Their wire bytes (`RaftMessage::encode`), before the group's envelope.
  pub(crate) bytes: u64,
  /// Each resolved isolation: when, and the node cut off.
  pub(crate) isolated: Vec<(u64, HostId)>,
  /// Each node's election timing at the end: base periods, span periods, broadcast round-trip tail.
  pub(crate) timings: BTreeMap<HostId, (u32, u32, u64)>,
  /// Leadership transfers started by priority (§3.4), over every node.
  pub(crate) priority_transfers: u64,
  /// Batches leaders sent ahead of acknowledgements (pipelined), over every node's life since its last
  /// restart.
  pub(crate) sent_ahead: u64,
  /// Stalled fast-track indices a leader filled (`RaftNode::fill_hole`).
  pub(crate) holes_filled: u64,
  /// Timeouts followers yielded to voters that outranked them.
  pub(crate) yields: u64,
}

impl Outcome {
  /// Leadership changes after the first leader: every leader event but the first.
  pub(crate) fn leader_changes(&self) -> usize {
    self.leader_events.len().saturating_sub(1)
  }

  /// Leadership changes at or after `since_ns`.
  pub(crate) fn leader_changes_after(&self, since_ns: u64) -> usize {
    self
      .leader_events
      .iter()
      .filter(|(at, _, _)| *at >= since_ns)
      .count()
  }

  /// The term of the last leader elected before `at_ns` (zero when none was).
  pub(crate) fn leader_term_at(&self, at_ns: u64) -> u64 {
    self
      .leader_events
      .iter()
      .rev()
      .find(|(at, _, _)| *at < at_ns)
      .map_or(0, |(_, _, term)| *term)
  }

  /// Campaigns begun at or after `since_ns`.
  pub(crate) fn campaigns_after(&self, since_ns: u64) -> usize {
    self
      .campaign_events
      .iter()
      .filter(|(at, _)| *at >= since_ns)
      .count()
  }

  /// The `p`th percentile (0–100) of commit latency, or zero with none.
  pub(crate) fn commit_latency_pct(&self, p: u64) -> u64 {
    let mut sorted = self.commit_latencies_ns.clone();
    sorted.sort_unstable();
    if sorted.is_empty() {
      return 0;
    }
    let rank = usize::try_from(p.min(100)).unwrap() * (sorted.len() - 1) / 100;
    sorted[rank]
  }
}

/// A splitmix64 generator, deterministic from its seed.
struct Rng(u64);

impl Rng {
  fn next(&mut self) -> u64 {
    self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = self.0;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
  }

  fn below(&mut self, bound: u64) -> u64 {
    if bound == 0 { 0 } else { self.next() % bound }
  }
}

/// What travels: a message, and for a request the time it was sent (its reply samples the round trip).
#[derive(Clone, Debug)]
struct Flight {
  from: HostId,
  to: HostId,
  message: RaftMessage,
  /// The send time of the request a reply answers (for the round-trip sample), else zero.
  request_sent_ns: u64,
  sent_ns: u64,
}

#[derive(Clone, Debug)]
enum Event {
  Tick(HostId),
  Deliver(Flight),
  /// A proposal a node forwarded to its leader (the classic path), arriving at `to`.
  Forward {
    to: HostId,
    command: Vec<u8>,
  },
  /// A probe's acknowledgement arriving back at `at`, timing the round trip to `peer`.
  ProbeAck {
    at: HostId,
    peer: HostId,
    sent_ns: u64,
  },
}

/// One simulated node: the real core, the real timer, the leader-contact count, the measured paths, a pending
/// invitation, and whether it is down.
struct Node {
  raft: RaftNode,
  timer: ElectionTimer,
  contact: u64,
  paths: BTreeMap<HostId, PathRtt>,
  /// When each peer last answered a probe: the node's liveness view, as SWIM's is.
  heard: BTreeMap<HostId, u64>,
  invitation: Option<TimeoutNow>,
  down_until: Option<u64>,
  retained: slates_cluster::raft::SavedRaft,
  /// While leading with the fast track open: the index it is stalled at and since when.
  stalled: Option<(u64, u64)>,
}

struct Sim {
  scenario: Scenario,
  now: u64,
  seq: u64,
  queue: BinaryHeap<Reverse<(u64, u64, usize)>>,
  events: Vec<Option<Event>>,
  nodes: BTreeMap<HostId, Node>,
  rng: Rng,
  outcome: Outcome,
  /// Proposals in flight: log index at the leader → the time it was proposed.
  proposed: BTreeMap<u64, u64>,
  /// A fixed proposer's proposals not yet committed: the time each was proposed → when it was last sent (zero
  /// when not yet).
  outstanding: BTreeMap<u64, u64>,
  next_proposal_ns: u64,
  last_commit_ns: u64,
  /// The highest index whose commit has been measured.
  measured_through: u64,
  /// Each isolation fault's cut side, once its start has resolved it.
  resolved: BTreeMap<usize, BTreeSet<HostId>>,
}

impl Sim {
  fn new(scenario: Scenario) -> Sim {
    let voters: Vec<HostId> = (1..=scenario.voters).map(HostId).collect();
    let nodes = voters
      .iter()
      .map(|id| {
        let mut raft = RaftNode::new(*id, voters.clone());
        raft.set_window_budget(initial_window(scenario.window, voters.len()));
        let retained = raft.saved();
        (
          *id,
          Node {
            raft,
            timer: ElectionTimer::new(),
            contact: 0,
            paths: BTreeMap::new(),
            heard: BTreeMap::new(),
            invitation: None,
            down_until: None,
            retained,
            stalled: None,
          },
        )
      })
      .collect();
    let rng = Rng(scenario.seed);
    let next_proposal_ns = scenario.propose_from_ns.max(scenario.propose_every_ns);
    Sim {
      scenario,
      now: 0,
      seq: 0,
      queue: BinaryHeap::new(),
      events: Vec::new(),
      nodes,
      rng,
      outcome: Outcome::default(),
      proposed: BTreeMap::new(),
      outstanding: BTreeMap::new(),
      next_proposal_ns,
      last_commit_ns: next_proposal_ns,
      measured_through: 0,
      resolved: BTreeMap::new(),
    }
  }

  fn schedule(&mut self, at: u64, event: Event) {
    self.seq += 1;
    self.events.push(Some(event));
    self
      .queue
      .push(Reverse((at, self.seq, self.events.len() - 1)));
  }

  fn partitioned(&self, from: HostId, to: HostId) -> bool {
    self
      .scenario
      .faults
      .iter()
      .enumerate()
      .any(|(index, fault)| {
        let (isolated, from_ns, until_ns) = match fault {
          Fault::Partition {
            isolated,
            from_ns,
            until_ns,
          } => (isolated, *from_ns, *until_ns),
          Fault::IsolateFollower { from_ns, until_ns }
          | Fault::IsolateLeader { from_ns, until_ns } => match self.resolved.get(&index) {
            Some(isolated) => (isolated, *from_ns, *until_ns),
            None => return false,
          },
          Fault::Crash { .. } => return false,
        };
        self.now >= from_ns
          && self.now < until_ns
          && isolated.contains(&from) != isolated.contains(&to)
      })
  }

  /// Resolves each isolation whose start has come: the lowest-id follower, or the leader, at that instant.
  fn resolve_isolations(&mut self) {
    let now = self.now;
    let leader = self
      .nodes
      .iter()
      .filter(|(id, node)| node.raft.is_leader() && node.down_until.is_none() && **id != HostId(0))
      .map(|(id, node)| (node.raft.term(), *id))
      .max()
      .map(|(_, id)| id);
    for (index, fault) in self.scenario.faults.iter().enumerate() {
      if self.resolved.contains_key(&index) {
        continue;
      }
      let chosen = match fault {
        Fault::IsolateFollower { from_ns, .. } if now >= *from_ns => {
          self.nodes.keys().copied().find(|id| Some(*id) != leader)
        }
        Fault::IsolateLeader { from_ns, .. } if now >= *from_ns => leader,
        _ => continue,
      };
      if let Some(node) = chosen {
        self.resolved.insert(index, BTreeSet::from([node]));
        self.outcome.isolated.push((now, node));
      }
    }
  }

  fn send(&mut self, from: HostId, to: HostId, message: RaftMessage, request_sent_ns: u64) {
    self.outcome.messages += 1;
    self.outcome.bytes += u64::try_from(message.encode().len()).unwrap();
    if self.partitioned(from, to)
      || self.rng.below(1_000_000) < u64::from(self.scenario.profile.loss_ppm)
    {
      return;
    }
    let (one_way, jitter) = self.scenario.profile.latency(from, to);
    let delay = one_way + self.rng.below(jitter);
    let flight = Flight {
      from,
      to,
      message,
      request_sent_ns,
      sent_ns: self.now,
    };
    self.schedule(self.now + delay, Event::Deliver(flight));
  }

  fn others(&self, of: HostId) -> Vec<HostId> {
    self.nodes[&of]
      .raft
      .all_voters()
      .into_iter()
      .filter(|voter| *voter != of)
      .collect()
  }

  fn timing(&self, of: HostId) -> ElectionTiming {
    let node = &self.nodes[&of];
    let others = self.others(of);
    ElectionTiming::derive(
      HEARTBEAT_NS,
      others.iter().filter_map(|peer| node.paths.get(peer)),
    )
  }

  /// Applies crash windows at the current time: a node going down, or coming back from what it retained.
  fn apply_crashes(&mut self) {
    let now = self.now;
    let windows: Vec<(HostId, u64, u64)> = self
      .scenario
      .faults
      .iter()
      .filter_map(|fault| match fault {
        Fault::Crash {
          node,
          from_ns,
          until_ns,
        } => Some((*node, *from_ns, *until_ns)),
        _ => None,
      })
      .collect();
    for (id, from, until) in windows {
      let node = self.nodes.get_mut(&id).unwrap();
      if now >= from && now < until && node.down_until.is_none() {
        node.retained = node.raft.saved();
        node.down_until = Some(until);
      } else if now >= until && node.down_until == Some(until) {
        node.raft = RaftNode::restore(node.retained.clone()).unwrap();
        // The window is configuration, not retained state; a derived one is set again at the next tick.
        let voters = node.raft.all_voters().len();
        node
          .raft
          .set_window_budget(initial_window(self.scenario.window, voters));
        node.timer = ElectionTimer::new();
        node.invitation = None;
        node.down_until = None;
      }
    }
  }

  fn down(&self, id: HostId) -> bool {
    self.nodes[&id].down_until.is_some()
  }

  /// Records a new leader and appends its current-term no-op (the council's rule, Raft §5.4.2).
  fn finish_election(&mut self, id: HostId, was_leader: bool) {
    let node = self.nodes.get_mut(&id).unwrap();
    if !was_leader && node.raft.is_leader() {
      node.raft.append_command(Vec::new());
      if self.scenario.fast_track {
        node.raft.open_fast_track();
      }
      let term = node.raft.term();
      self.outcome.leader_events.push((self.now, id, term));
      // Proposals the old leader held are abandoned; the stream restarts at the new leader.
      self.proposed.clear();
      self.measured_through = node.raft.commit_index();
    }
  }

  fn broadcast(&mut self, from: HostId, messages: Vec<RaftMessage>) {
    let others = self.others(from);
    for (to, message) in others.into_iter().zip(messages) {
      self.send(from, to, message, self.now);
    }
  }

  fn tick(&mut self, id: HostId) {
    self.schedule(self.now + HEARTBEAT_NS, Event::Tick(id));
    if self.down(id) {
      return;
    }
    self.probe(id);
    // This period's election priority, as the daemon's drive measures it (§3.4) — or unknown in the control.
    let node = &self.nodes[&id];
    let voters = node.raft.all_voters();
    let priority = match self.scenario.order {
      ElectionOrder::ByPriority => quorum_priority(
        voters
          .iter()
          .filter(|voter| **voter != id)
          .map(|voter| node.paths.get(voter)),
        voters.len(),
      ),
      ElectionOrder::ByTimeout => ElectionPriority::default(),
    };
    self.nodes.get_mut(&id).unwrap().raft.set_priority(priority);
    let timing = self.timing(id);
    if self.scenario.window == Window::Derived {
      let window = timing.window_budget(HEARTBEAT_NS, batch_budget(voters.len()));
      self
        .nodes
        .get_mut(&id)
        .unwrap()
        .raft
        .set_window_budget(window);
    }
    if self.scenario.proposer == Proposer::At(id) {
      self.propose_at(id, &timing);
    }
    if self.nodes[&id].raft.is_leader() {
      self.lead(id, &timing);
    } else {
      self.follow(id, &timing);
    }
  }

  /// The daemon's SWIM probe, as far as election timing sees it: every period each node pings every peer,
  /// and the acknowledged round trip feeds its path estimate ([`PathRtt`]) — so every node's timing is derived
  /// from many samples, as the daemon's is, not only a leader's or candidate's. Both legs cross the modelled
  /// network: latency and jitter each way, loss, partitions.
  fn probe(&mut self, id: HostId) {
    for peer in self.others(id) {
      if self.down(peer) || self.partitioned(id, peer) || self.partitioned(peer, id) {
        continue;
      }
      let loss = u64::from(self.scenario.profile.loss_ppm);
      if self.rng.below(1_000_000) < loss || self.rng.below(1_000_000) < loss {
        continue;
      }
      let (out, out_jitter) = self.scenario.profile.latency(id, peer);
      let (back, back_jitter) = self.scenario.profile.latency(peer, id);
      let round_trip = out + self.rng.below(out_jitter) + back + self.rng.below(back_jitter);
      self.schedule(
        self.now + round_trip,
        Event::ProbeAck {
          at: id,
          peer,
          sent_ns: self.now,
        },
      );
    }
  }

  /// The peers `id` holds alive: each answered a probe within the suspicion span — probes go every period
  /// and are answered in a stream however long the path, so a live peer's last answer is at most about a
  /// period old; [`ALIVE_PERIODS`] of silence is the daemon's suspicion (two periods) and one more for the
  /// probe interval.
  fn alive(&self, id: HostId) -> Vec<HostId> {
    self.nodes[&id]
      .heard
      .iter()
      .filter(|(_, at)| self.now.saturating_sub(**at) <= ALIVE_PERIODS * HEARTBEAT_NS)
      .map(|(peer, _)| *peer)
      .collect()
  }

  fn lead(&mut self, id: HostId, timing: &ElectionTiming) {
    if self.scenario.proposer == Proposer::Leader {
      self.propose_due(id);
    }
    self.fill_a_stalled_hole(id, timing);
    let budget = batch_budget(self.nodes[&id].raft.all_voters().len());
    for peer in self.others(id) {
      let raft = &mut self.nodes.get_mut(&id).unwrap().raft;
      let message = raft
        .replicate_to(peer, budget)
        .map(RaftMessage::AppendEntries)
        .or_else(|| {
          raft
            .install_snapshot_for(peer)
            .map(RaftMessage::InstallSnapshot)
        });
      if let Some(message) = message {
        self.send(id, peer, message, self.now);
      }
    }
    let alive = self.alive(id);
    let node = self.nodes.get_mut(&id).unwrap();
    let invitation = node.raft.take_timeout_now();
    if node.timer.leader_period(timing) {
      node.raft.check_quorum();
      // A voter that commits distinguishably faster takes over (§3.4); its invitation leaves next period.
      node.raft.priority_transfer(&alive);
    }
    if let Some((to, invitation)) = invitation {
      self.send(id, to, RaftMessage::TimeoutNow(invitation), self.now);
    }
  }

  /// A fixed proposer's period: every command due since its last period joins the outstanding ones, and each
  /// outstanding command not sent within [`REPAIR_ROUND_TRIPS`] round trips of its slowest voter path (at
  /// least a period) is sent: on the fast track when it is open to this node — to every voter, its own vote
  /// cast here — and otherwise to its leader.
  fn propose_at(&mut self, id: HostId, timing: &ElectionTiming) {
    let every = self.scenario.propose_every_ns;
    if every > 0 {
      while self.next_proposal_ns <= self.now {
        self.outstanding.insert(self.next_proposal_ns, 0);
        self.next_proposal_ns += every;
      }
    }
    let resend_after = timing
      .broadcast_rtt_tail_ns
      .saturating_mul(REPAIR_ROUND_TRIPS)
      .max(HEARTBEAT_NS);
    let now = self.now;
    let due: Vec<u64> = self
      .outstanding
      .iter()
      .filter(|(_, sent)| **sent == 0 || now.saturating_sub(**sent) >= resend_after)
      .map(|(proposed, _)| *proposed)
      .collect();
    for proposed in due {
      self.outstanding.insert(proposed, now);
      let command = proposed.to_le_bytes().to_vec();
      let node = self.nodes.get_mut(&id).unwrap();
      if let Some(proposal) = node.raft.propose_fast(command.clone()) {
        let others = self.others(id);
        for voter in others {
          self.send(id, voter, RaftMessage::FastPropose(proposal.clone()), 0);
        }
        let node = self.nodes.get_mut(&id).unwrap();
        if let Some(vote) = node.raft.on_fast_propose(proposal) {
          self.route_vote(id, vote);
        }
      } else if node.raft.is_leader() {
        node.raft.append_command(command);
      } else if let Some(leader) = node.raft.leader() {
        self.forward(id, leader, command);
      }
    }
  }

  /// The leader's fast track, stalled at an index for as long as a lost vote takes to be sent again
  /// ([`REPAIR_ROUND_TRIPS`] round trips of its slowest voter path, at least a period), is filled there: the
  /// leader proposes a no-op at the index to every voter, its own vote cast here (`RaftNode::fill_hole`).
  fn fill_a_stalled_hole(&mut self, id: HostId, timing: &ElectionTiming) {
    let now = self.now;
    let repair = timing
      .broadcast_rtt_tail_ns
      .saturating_mul(REPAIR_ROUND_TRIPS)
      .max(HEARTBEAT_NS);
    let node = self.nodes.get_mut(&id).unwrap();
    let Some(index) = node.raft.stalled_index() else {
      node.stalled = None;
      return;
    };
    match node.stalled {
      Some((at, since)) if at == index && now.saturating_sub(since) >= repair => {}
      Some((at, _)) if at == index => return,
      _ => {
        node.stalled = Some((index, now));
        return;
      }
    }
    node.stalled = Some((index, now));
    let Some(fill) = node.raft.fill_hole(index) else {
      return;
    };
    self.outcome.holes_filled += 1;
    for voter in self.others(id) {
      self.send(id, voter, RaftMessage::FastPropose(fill.clone()), 0);
    }
    let node = self.nodes.get_mut(&id).unwrap();
    if let Some(vote) = node.raft.on_fast_propose(fill) {
      self.route_vote(id, vote);
    }
  }

  /// Sends a fast vote cast at `voter` to its leader — the leader tallies its own at once.
  fn route_vote(&mut self, voter: HostId, vote: slates_cluster::raft::FastVote) {
    let Some(leader) = self.nodes[&voter].raft.leader() else {
      return;
    };
    if leader == voter {
      self.nodes.get_mut(&voter).unwrap().raft.on_fast_vote(vote);
      self.measure_commits(voter);
    } else {
      self.send(voter, leader, RaftMessage::FastVote(vote), 0);
    }
  }

  /// Forwards a proposal from `from` to its leader `to`, over the path between them (lossy, as any message).
  fn forward(&mut self, from: HostId, to: HostId, command: Vec<u8>) {
    self.outcome.messages += 1;
    if self.partitioned(from, to)
      || self.rng.below(1_000_000) < u64::from(self.scenario.profile.loss_ppm)
    {
      return;
    }
    let (one_way, jitter) = self.scenario.profile.latency(from, to);
    let delay = one_way + self.rng.below(jitter);
    self.schedule(self.now + delay, Event::Forward { to, command });
  }

  /// The leader proposes every command due since its last proposal (bounded by the stream's cadence).
  fn propose_due(&mut self, id: HostId) {
    let every = self.scenario.propose_every_ns;
    if every == 0 {
      return;
    }
    while self.next_proposal_ns <= self.now {
      let node = self.nodes.get_mut(&id).unwrap();
      if node
        .raft
        .append_command(self.next_proposal_ns.to_le_bytes().to_vec())
      {
        let index = node.raft.last_log_index();
        self.proposed.insert(index, self.next_proposal_ns);
      }
      self.next_proposal_ns += every;
    }
  }

  fn follow(&mut self, id: HostId, timing: &ElectionTiming) {
    let was_leader = false;
    if let Some(invitation) = self.nodes.get_mut(&id).unwrap().invitation.take() {
      let votes = self
        .nodes
        .get_mut(&id)
        .unwrap()
        .raft
        .on_timeout_now(invitation);
      if !votes.is_empty() {
        self.outcome.campaigns += 1;
        self.outcome.campaign_events.push((self.now, id));
        self.finish_election(id, was_leader);
        self.broadcast(
          id,
          votes.into_iter().map(RaftMessage::RequestVote).collect(),
        );
        let node = self.nodes.get_mut(&id).unwrap();
        let contact = node.contact;
        node.timer.rebaseline(contact);
        return;
      }
    }
    let alive = self.alive(id);
    let node = self.nodes.get_mut(&id).unwrap();
    let contact = node.contact;
    let rank = node.raft.election_rank(&alive);
    let yielded = node.timer.yielded();
    let campaign = node.timer.follower_period(contact, timing, id, rank);
    if node.timer.yielded() > yielded {
      self.outcome.yields += 1;
    }
    if !campaign {
      return;
    }
    let node = self.nodes.get_mut(&id).unwrap();
    self.outcome.campaigns += 1;
    self.outcome.campaign_events.push((self.now, id));
    let messages: Vec<RaftMessage> = match self.scenario.campaign {
      Campaign::PreVote => node
        .raft
        .on_election_timeout()
        .into_iter()
        .map(RaftMessage::PreVote)
        .collect(),
      Campaign::Direct => node
        .raft
        .start_election()
        .into_iter()
        .map(RaftMessage::RequestVote)
        .collect(),
    };
    let contact = node.contact;
    node.timer.rebaseline(contact);
    self.finish_election(id, was_leader);
    self.broadcast(id, messages);
  }

  fn deliver(&mut self, flight: Flight) {
    let to = flight.to;
    if self.down(to) {
      return;
    }
    let was_leader = self.nodes[&to].raft.is_leader();
    let answers_a_request = matches!(flight.message_kind(), Kind::Reply);
    let proposal = matches!(flight.message, RaftMessage::FastPropose(_));
    let node = self.nodes.get_mut(&to).unwrap();
    let (reply, follow_on) = answer(node, flight.message);
    // A reply's round trip samples the path to the peer that answered (the daemon samples the same round
    // trips its election timing derives from).
    if flight.request_sent_ns > 0 && answers_a_request {
      let round_trip = self.now.saturating_sub(flight.request_sent_ns);
      self
        .nodes
        .get_mut(&to)
        .unwrap()
        .paths
        .entry(flight.from)
        .or_default()
        .on_sample(round_trip);
    }
    self.finish_election(to, was_leader);
    match reply {
      // A fast vote goes to the voter's leader, whoever proposed (§3.7).
      Some(RaftMessage::FastVote(vote)) if proposal => self.route_vote(to, vote),
      Some(reply) => self.send(to, flight.from, reply, flight.sent_ns),
      None => {}
    }
    if !follow_on.is_empty() {
      self.broadcast(to, follow_on);
    }
    self.measure_commits(to);
  }

  /// Folds the leader's commit progress into the latencies of the proposals it covers.
  fn measure_commits(&mut self, id: HostId) {
    let node = &self.nodes[&id];
    if !node.raft.is_leader() {
      return;
    }
    if let Proposer::At(proposer) = self.scenario.proposer {
      self.measure_commits_for(id, proposer);
      return;
    }
    let committed = node.raft.commit_index();
    if committed <= self.measured_through {
      return;
    }
    let done: Vec<u64> = self
      .proposed
      .range(..=committed)
      .map(|(index, _)| *index)
      .collect();
    for index in done {
      if let Some(at) = self.proposed.remove(&index) {
        self
          .outcome
          .commit_latencies_ns
          .push(self.now.saturating_sub(at));
        // Unavailability is the time between successive commits: proposals arrive every cadence, so any
        // longer wait is time the group could not commit (measuring against the proposal's own time would
        // hide a leaderless window behind proposals made after it).
        let gap = self.now.saturating_sub(self.last_commit_ns);
        self.outcome.longest_gap_ns = self.outcome.longest_gap_ns.max(gap);
        self.last_commit_ns = self.now;
      }
    }
    self.measured_through = committed;
  }

  /// The leader `id`'s commits, fast ones included, of a fixed proposer's commands: each command carries the
  /// time it was proposed, so its latency is known wherever it landed — to the leader's commit and on along the
  /// leader's one-way path to the proposer, which learns it then.
  fn measure_commits_for(&mut self, id: HostId, proposer: HostId) {
    let node = &self.nodes[&id];
    let through = node.raft.committed_through();
    if through <= self.measured_through {
      return;
    }
    let base = node.raft.snapshot_index();
    let from = usize::try_from(self.measured_through.saturating_sub(base)).unwrap();
    let times: Vec<u64> = node
      .raft
      .committed_entries()
      .get(from..)
      .unwrap_or(&[])
      .iter()
      .filter_map(|entry| <[u8; 8]>::try_from(entry.command.as_slice()).ok())
      .map(u64::from_le_bytes)
      .collect();
    let back = if proposer == id {
      0
    } else {
      self.scenario.profile.latency(id, proposer).0
    };
    for proposed in times {
      if self.outstanding.remove(&proposed).is_some() {
        let learnt = self.now.saturating_add(back);
        self
          .outcome
          .commit_latencies_ns
          .push(learnt.saturating_sub(proposed));
        let gap = learnt.saturating_sub(self.last_commit_ns);
        self.outcome.longest_gap_ns = self.outcome.longest_gap_ns.max(gap);
        self.last_commit_ns = self.last_commit_ns.max(learnt);
      }
    }
    self.measured_through = through;
  }

  fn run(mut self) -> Outcome {
    let ids: Vec<HostId> = self.nodes.keys().copied().collect();
    for id in ids {
      let phase = self.rng.below(HEARTBEAT_NS);
      self.schedule(phase, Event::Tick(id));
    }
    while let Some(Reverse((at, _, slot))) = self.queue.pop() {
      if at >= self.scenario.duration_ns {
        break;
      }
      self.now = at;
      self.apply_crashes();
      self.resolve_isolations();
      match self.events[slot].take() {
        Some(Event::Tick(id)) => self.tick(id),
        Some(Event::Deliver(flight)) => self.deliver(flight),
        Some(Event::ProbeAck { at, peer, sent_ns }) if !self.down(at) => {
          let round_trip = self.now.saturating_sub(sent_ns);
          let now = self.now;
          let node = self.nodes.get_mut(&at).unwrap();
          node.paths.entry(peer).or_default().on_sample(round_trip);
          node.heard.insert(peer, now);
        }
        Some(Event::Forward { to, command }) if !self.down(to) => {
          let node = self.nodes.get_mut(&to).unwrap();
          if node.raft.is_leader() {
            node.raft.append_command(command);
          }
        }
        Some(Event::ProbeAck { .. } | Event::Forward { .. }) | None => {}
      }
    }
    for (id, node) in &self.nodes {
      self.outcome.priority_transfers += node.raft.priority_transfers();
      self.outcome.sent_ahead += node.raft.window_counters().sent_ahead;
      self.outcome.final_terms.insert(*id, node.raft.term());
      self.outcome.max_term = self.outcome.max_term.max(node.raft.term());
    }
    // No commit since the last one to the end is unavailability too.
    let tail = self
      .scenario
      .duration_ns
      .saturating_sub(self.last_commit_ns);
    self.outcome.longest_gap_ns = self.outcome.longest_gap_ns.max(tail);
    for id in self.nodes.keys() {
      let timing = self.timing(*id);
      self.outcome.timings.insert(
        *id,
        (
          timing.base_periods,
          timing.span_periods,
          timing.broadcast_rtt_tail_ns,
        ),
      );
    }
    self.outcome
  }
}

/// The council drive's batch budget for a group of `voters`: a fresh fleet session's first credit, less the
/// append header and the group envelope (a 32-byte group id and a 4-byte length).
pub(crate) fn batch_budget(voters: usize) -> usize {
  append_batch_bytes(MAX_PACKET_PAYLOAD, ENVELOPE_BYTES, voters)
}

/// A node's window before it has measured anything: the fixed bytes, or — derived — the timing floor's, one
/// batch.
fn initial_window(window: Window, voters: usize) -> usize {
  match window {
    Window::Bytes(bytes) => bytes,
    Window::Derived => ElectionTiming::floor().window_budget(HEARTBEAT_NS, batch_budget(voters)),
  }
}

/// `node` handles one delivered `message` as the council's serve and fold paths do: the reply a request
/// earns, and the vote requests a won pre-election yields. A leader's append or snapshot, and a vote this
/// node granted, count as contact (the timer rules of Raft Figure 2).
fn answer(node: &mut Node, message: RaftMessage) -> (Option<RaftMessage>, Vec<RaftMessage>) {
  match message {
    RaftMessage::PreVote(pre) => (
      Some(RaftMessage::PreVoteReply(node.raft.on_pre_vote(pre))),
      Vec::new(),
    ),
    RaftMessage::RequestVote(vote) => {
      let answer = node.raft.on_request_vote(vote);
      if answer.granted {
        node.contact += 1;
      }
      (Some(RaftMessage::VoteReply(answer)), Vec::new())
    }
    RaftMessage::AppendEntries(append) => {
      let append_term = append.term;
      let answer = node.raft.on_append_entries(append);
      if append_term >= answer.term {
        node.contact += 1;
      }
      (Some(RaftMessage::AppendReply(answer)), Vec::new())
    }
    RaftMessage::InstallSnapshot(snapshot) => {
      let snapshot_term = snapshot.term;
      let answer = node.raft.on_install_snapshot(snapshot);
      if snapshot_term >= answer.term {
        node.contact += 1;
      }
      (Some(RaftMessage::InstallSnapshotReply(answer)), Vec::new())
    }
    RaftMessage::TimeoutNow(invitation) => {
      node.invitation = Some(invitation);
      (None, Vec::new())
    }
    RaftMessage::PreVoteReply(answer) => (
      None,
      node
        .raft
        .on_pre_vote_reply(answer)
        .map(|votes| votes.into_iter().map(RaftMessage::RequestVote).collect())
        .unwrap_or_default(),
    ),
    RaftMessage::VoteReply(answer) => {
      node.raft.on_vote_reply(answer);
      (None, Vec::new())
    }
    RaftMessage::AppendReply(answer) => {
      node.raft.on_append_reply(answer);
      (None, Vec::new())
    }
    // The fast track (research record §3.7): a proposal is answered with the voter's vote; a vote the leader
    // receives is tallied.
    RaftMessage::FastPropose(proposal) => (
      node
        .raft
        .on_fast_propose(proposal)
        .map(RaftMessage::FastVote),
      Vec::new(),
    ),
    RaftMessage::FastVote(vote) => {
      node.raft.on_fast_vote(vote);
      (None, Vec::new())
    }
    RaftMessage::InstallSnapshotReply(answer) => {
      node.raft.on_install_snapshot_reply(answer);
      (None, Vec::new())
    }
  }
}

/// A message's kind: a request expects a reply; a reply answers one; an invitation expects none.
enum Kind {
  Request,
  Reply,
}

impl Flight {
  fn message_kind(&self) -> Kind {
    match self.message {
      RaftMessage::PreVoteReply(_)
      | RaftMessage::VoteReply(_)
      | RaftMessage::AppendReply(_)
      | RaftMessage::InstallSnapshotReply(_) => Kind::Reply,
      _ => Kind::Request,
    }
  }
}

/// Runs `scenario` to its end and returns what it measured.
pub(crate) fn run(scenario: Scenario) -> Outcome {
  Sim::new(scenario).run()
}
