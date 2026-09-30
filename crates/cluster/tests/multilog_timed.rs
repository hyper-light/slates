//! MLRaft on the failure path (`docs/wip/research/consensus-enhancements.md` §1 and §3.6: a mechanism whose
//! only gain is throughput is on by default only for its measured gain on the failure path). A timed,
//! seeded, discrete-event simulation of `n`-log groups (`slates_cluster::multilog::MultiLog`) across the five
//! published Azure regions (`support::azure`), driven as the council's drive drives one log: each period every
//! node probes its peers (the measured paths the election timing and priorities derive from), each log's
//! leader replicates and judges its quorum, each log's followers age their own election timers, every leader
//! appends the barriers its logs owe, and the logs' leaders are spread by priority (log `k` prefers the voter
//! ranked `k`). A keyed stream and a global stream of commands are proposed at the leader of the log each
//! routes to; a command's latency runs to its application there, through the merge.
//!
//! Measured: what a global command pays for its barriers as `n` grows, what a keyed command pays, and what a
//! crash of each log's leader costs each. At `n = 1` it measures what the single-log simulation
//! (`support::timed`) measures of the same group: a 174 ms median over 20 seeds here, 171 ms there
//! (`tests/priority.rs`, priority elections over the same regions). Test by use (R5).

// Test harness: an unwrap, expect or panic here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};

use slates_cluster::multilog::{MultiLog, Route};
use slates_cluster::raft::SavedRaft;
use slates_cluster::raft_wire::RaftMessage;
use slates_cluster::timing::{
  ElectionTimer, ElectionTiming, FollowerStep, PathRtt, quorum_priority,
};
use slates_db::register::HostId;
use support::azure::{REGIONS, placement, profile, quorum_round_trip_ms};
use support::timed::{HEARTBEAT_NS, MS, Profile};

/// Shape: the jitter added to each one-way delay.
const JITTER_NS: u64 = 5 * MS;
/// Shape: when the streams begin — after the first elections on these paths.
const STREAM_FROM_NS: u64 = 10_000 * MS;
/// Shape: the keyed stream's cadence — twenty a second — and the keys it writes.
const KEYED_EVERY_NS: u64 = 50 * MS;
const KEYS: u64 = 64;
/// Shape: the global stream's cadence — two a second, the rate of the groups' membership changes under churn.
const GLOBAL_EVERY_NS: u64 = 500 * MS;

/// A run's shape.
#[derive(Clone, Copy, Debug)]
struct Shape {
  logs: usize,
  seed: u64,
  duration_ns: u64,
  crash: Option<Crash>,
}

/// A crash of the voter log `log` prefers — its leader, once the priorities have moved it there — over
/// `[from_ns, until_ns)`, and a restart from what it retained.
#[derive(Clone, Copy, Debug)]
struct Crash {
  from_ns: u64,
  until_ns: u64,
  log: usize,
}

/// A message in flight in one log.
struct Flight {
  log: usize,
  from: HostId,
  to: HostId,
  message: RaftMessage,
}

enum Event {
  Tick(HostId),
  Deliver(Flight),
  ProbeAck {
    at: HostId,
    peer: HostId,
    sent_ns: u64,
  },
}

/// One node: its logs, per log the election timer and the leader contacts it counts, its measured paths, and
/// what it retained.
struct Node {
  multi: MultiLog,
  timers: Vec<ElectionTimer>,
  contacts: Vec<u64>,
  paths: BTreeMap<HostId, PathRtt>,
  retained: Vec<SavedRaft>,
  down_until: Option<u64>,
}

/// What a run measured: each keyed and global command's latency to its application, the longest time either
/// stream went without an application — and the keyed commands of the watched log alone, the log whose
/// leader the crash takes (log 0 in a steady run) — and the messages sent.
#[derive(Default, Debug)]
struct Outcome {
  keyed_ns: Vec<u64>,
  global_ns: Vec<u64>,
  keyed_gap_ns: u64,
  watched_keyed_gap_ns: u64,
  global_gap_ns: u64,
  messages: u64,
}

struct Sim {
  shape: Shape,
  profile: Profile,
  hosts: Vec<HostId>,
  ranked: Vec<HostId>,
  now: u64,
  seq: u64,
  queue: BinaryHeap<Reverse<(u64, u64, usize)>>,
  events: Vec<Option<Event>>,
  nodes: BTreeMap<HostId, Node>,
  rng: u64,
  next_keyed_ns: u64,
  next_global_ns: u64,
  /// Proposals not yet appended (their log had no leader): (route, proposal time).
  waiting: Vec<(Route, u64)>,
  last_keyed_ns: u64,
  last_watched_keyed_ns: u64,
  last_global_ns: u64,
  outcome: Outcome,
}

impl Sim {
  fn new(shape: Shape) -> Sim {
    let hosts = placement(REGIONS.len(), shape.seed);
    // Ranked best first by the published quorum round trip: log k prefers the voter at place k.
    let mut order: Vec<usize> = (0..REGIONS.len()).collect();
    order.sort_by_key(|region| (quorum_round_trip_ms(*region, REGIONS.len()), *region));
    let ranked: Vec<HostId> = order.iter().map(|region| hosts[*region]).collect();
    let voters: Vec<HostId> = (1..=u64::try_from(REGIONS.len()).unwrap())
      .map(HostId)
      .collect();
    let nodes = voters
      .iter()
      .map(|id| {
        let multi = MultiLog::new(*id, voters.clone(), shape.logs);
        let retained = multi.saved();
        (
          *id,
          Node {
            multi,
            timers: (0..shape.logs).map(|_| ElectionTimer::new()).collect(),
            contacts: vec![0; shape.logs],
            paths: BTreeMap::new(),
            retained,
            down_until: None,
          },
        )
      })
      .collect();
    Sim {
      profile: profile(&hosts, JITTER_NS, 0),
      hosts,
      ranked,
      shape,
      now: 0,
      seq: 0,
      queue: BinaryHeap::new(),
      events: Vec::new(),
      nodes,
      rng: shape.seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1,
      next_keyed_ns: STREAM_FROM_NS,
      next_global_ns: STREAM_FROM_NS,
      waiting: Vec::new(),
      last_keyed_ns: STREAM_FROM_NS,
      last_watched_keyed_ns: STREAM_FROM_NS,
      last_global_ns: STREAM_FROM_NS,
      outcome: Outcome::default(),
    }
  }

  fn draw(&mut self, bound: u64) -> u64 {
    self.rng ^= self.rng << 13;
    self.rng ^= self.rng >> 7;
    self.rng ^= self.rng << 17;
    self.rng % bound.max(1)
  }

  fn schedule(&mut self, at: u64, event: Event) {
    self.seq += 1;
    self.events.push(Some(event));
    self
      .queue
      .push(Reverse((at, self.seq, self.events.len() - 1)));
  }

  fn latency(&mut self, from: HostId, to: HostId) -> u64 {
    let (one_way, jitter) = self
      .profile
      .pairs
      .get(&(from, to))
      .copied()
      .unwrap_or((0, 0));
    one_way + self.draw(jitter)
  }

  fn send(&mut self, log: usize, from: HostId, to: HostId, message: RaftMessage) {
    self.outcome.messages += 1;
    let delay = self.latency(from, to);
    self.schedule(
      self.now + delay,
      Event::Deliver(Flight {
        log,
        from,
        to,
        message,
      }),
    );
  }

  fn down(&self, id: HostId) -> bool {
    self.nodes[&id].down_until.is_some()
  }

  fn others(&self, id: HostId) -> Vec<HostId> {
    self
      .nodes
      .keys()
      .copied()
      .filter(|other| *other != id)
      .collect()
  }

  fn retain(&mut self, id: HostId) {
    let node = self.nodes.get_mut(&id).unwrap();
    if node.multi.retention_pending() {
      node.retained = node.multi.saved();
      node.multi.retained();
    }
  }

  /// The crash window, applied at the current time to the voter the crashed log prefers.
  fn apply_crash(&mut self) {
    let Some(Crash {
      from_ns: from,
      until_ns: until,
      log,
    }) = self.shape.crash
    else {
      return;
    };
    let target = self.ranked[log % self.ranked.len()];
    let now = self.now;
    let node = self.nodes.get_mut(&target).unwrap();
    if now >= from && now < until && node.down_until.is_none() {
      node.down_until = Some(until);
    } else if now >= until && node.down_until == Some(until) {
      node.multi = MultiLog::restore(node.retained.clone()).unwrap();
      node.timers = (0..self.shape.logs).map(|_| ElectionTimer::new()).collect();
      node.down_until = None;
    }
  }

  fn tick(&mut self, id: HostId) {
    self.schedule(self.now + HEARTBEAT_NS, Event::Tick(id));
    if self.down(id) {
      return;
    }
    for peer in self.others(id) {
      if self.down(peer) {
        continue;
      }
      let there = self.latency(id, peer);
      let back = self.latency(peer, id);
      let sent_ns = self.now;
      self.schedule(
        self.now + there + back,
        Event::ProbeAck {
          at: id,
          peer,
          sent_ns,
        },
      );
    }
    let node = &self.nodes[&id];
    let others = self.others(id);
    let timing = ElectionTiming::derive(
      HEARTBEAT_NS,
      others.iter().filter_map(|peer| node.paths.get(peer)),
    );
    let measured = quorum_priority(
      others.iter().map(|peer| node.paths.get(peer)),
      others.len() + 1,
    );
    let ranked = self.ranked.clone();
    self
      .nodes
      .get_mut(&id)
      .unwrap()
      .multi
      .set_priorities(measured, &ranked);
    for log in 0..self.shape.logs {
      if self.nodes[&id].multi.log(log).unwrap().is_leader() {
        self.lead(id, log, &timing);
      } else {
        self.follow(id, log, &timing);
      }
    }
    self.nodes.get_mut(&id).unwrap().multi.append_barriers();
    self.propose_due();
    self.retain(id);
    self.apply(id);
  }

  fn lead(&mut self, id: HostId, log: usize, timing: &ElectionTiming) {
    for peer in self.others(id) {
      let raft = self.nodes.get_mut(&id).unwrap().multi.log_mut(log).unwrap();
      if let Some(append) = raft.replicate_to(peer, usize::MAX) {
        self.send(log, id, peer, RaftMessage::AppendEntries(append));
      }
    }
    let alive: Vec<HostId> = self
      .nodes
      .keys()
      .copied()
      .filter(|other| !self.down(*other))
      .collect();
    let node = self.nodes.get_mut(&id).unwrap();
    let raft = node.multi.log_mut(log).unwrap();
    let invitation = raft.take_timeout_now();
    if node.timers[log].leader_period(timing) {
      let raft = node.multi.log_mut(log).unwrap();
      raft.check_quorum();
      raft.priority_transfer(&alive);
    }
    if let Some((to, invitation)) = invitation {
      self.send(log, id, to, RaftMessage::TimeoutNow(invitation));
    }
  }

  fn follow(&mut self, id: HostId, log: usize, timing: &ElectionTiming) {
    let alive: Vec<HostId> = self
      .nodes
      .keys()
      .copied()
      .filter(|other| !self.down(*other))
      .collect();
    let node = self.nodes.get_mut(&id).unwrap();
    let contact = node.contacts[log];
    let rank = node.multi.log(log).unwrap().election_rank(&alive);
    match node.timers[log].follower_period(contact, timing, id, rank) {
      FollowerStep::Follow => return,
      FollowerStep::LeaderLapsed => {
        node.multi.log_mut(log).unwrap().forget_leader();
        return;
      }
      FollowerStep::Campaign => {}
    }
    let pre_votes = node
      .multi
      .log_mut(log)
      .unwrap()
      .on_election_timeout()
      .unwrap();
    node.timers[log].rebaseline(contact);
    self.finish_election(id, log, false);
    for (to, pre_vote) in self.others(id).into_iter().zip(pre_votes) {
      self.send(log, id, to, RaftMessage::PreVote(pre_vote));
    }
  }

  /// A new leader of `log` appends its no-op (the council's rule, Raft §5.4.2).
  fn finish_election(&mut self, id: HostId, log: usize, was_leader: bool) {
    let raft = self.nodes.get_mut(&id).unwrap().multi.log_mut(log).unwrap();
    if !was_leader && raft.is_leader() {
      raft.append_command(Vec::new());
    }
  }

  /// Proposes every command due, and those still waiting, at the leader of the log each routes to.
  fn propose_due(&mut self) {
    while self.next_keyed_ns <= self.now {
      let key = self.draw(KEYS);
      self.waiting.push((Route::Key(key), self.next_keyed_ns));
      self.next_keyed_ns += KEYED_EVERY_NS;
    }
    while self.next_global_ns <= self.now {
      self.waiting.push((Route::Global, self.next_global_ns));
      self.next_global_ns += GLOBAL_EVERY_NS;
    }
    let waiting = std::mem::take(&mut self.waiting);
    for (route, proposed) in waiting {
      let log = self.nodes.values().next().unwrap().multi.route(route);
      let leader = self
        .nodes
        .iter()
        .find(|(id, node)| !self.down(**id) && node.multi.log(log).unwrap().is_leader())
        .map(|(id, _)| *id);
      let appended = leader.is_some_and(|leader| {
        let proposed_bytes = proposed.to_le_bytes().to_vec();
        self
          .nodes
          .get_mut(&leader)
          .unwrap()
          .multi
          .propose(route, proposed_bytes)
      });
      if !appended {
        self.waiting.push((route, proposed));
      }
    }
  }

  /// Node `id` applies what its logs allow; a command applied at the leader that appended it is measured
  /// there (each is applied once per node; the first node to apply a command is its leader or one after it).
  fn apply(&mut self, id: HostId) {
    let applied = self.nodes.get_mut(&id).unwrap().multi.apply_ready();
    for applied in applied {
      let led = self.nodes[&id].multi.log(applied.log).unwrap().is_leader();
      let Ok(bytes) = <[u8; 8]>::try_from(applied.command.as_slice()) else {
        continue;
      };
      if !led {
        continue;
      }
      let proposed = u64::from_le_bytes(bytes);
      let latency = self.now.saturating_sub(proposed);
      if applied.key.is_some() {
        self.outcome.keyed_ns.push(latency);
        self.outcome.keyed_gap_ns = self
          .outcome
          .keyed_gap_ns
          .max(self.now.saturating_sub(self.last_keyed_ns));
        self.last_keyed_ns = self.last_keyed_ns.max(self.now);
        if applied.log == self.shape.crash.map_or(0, |crash| crash.log) {
          self.outcome.watched_keyed_gap_ns = self
            .outcome
            .watched_keyed_gap_ns
            .max(self.now.saturating_sub(self.last_watched_keyed_ns));
          self.last_watched_keyed_ns = self.last_watched_keyed_ns.max(self.now);
        }
      } else {
        self.outcome.global_ns.push(latency);
        self.outcome.global_gap_ns = self
          .outcome
          .global_gap_ns
          .max(self.now.saturating_sub(self.last_global_ns));
        self.last_global_ns = self.last_global_ns.max(self.now);
      }
    }
  }

  fn deliver(&mut self, flight: Flight) {
    let to = flight.to;
    if self.down(to) {
      return;
    }
    let log = flight.log;
    let node = self.nodes.get_mut(&to).unwrap();
    let raft = node.multi.log_mut(log).unwrap();
    let was_leader = raft.is_leader();
    let mut reply = None;
    let mut votes = Vec::new();
    match flight.message {
      RaftMessage::PreVote(pre) => reply = Some(RaftMessage::PreVoteReply(raft.on_pre_vote(pre))),
      RaftMessage::RequestVote(vote) => {
        let answer = raft.on_request_vote(vote);
        if answer.granted {
          node.contacts[log] += 1;
        }
        reply = Some(RaftMessage::VoteReply(answer));
      }
      RaftMessage::AppendEntries(append) => {
        let term = append.term;
        let answer = raft.on_append_entries(append);
        if term >= answer.term {
          node.contacts[log] += 1;
        }
        reply = Some(RaftMessage::AppendReply(answer));
      }
      RaftMessage::TimeoutNow(invitation) => votes = raft.on_timeout_now(invitation),
      RaftMessage::PreVoteReply(answer) => {
        votes = raft.on_pre_vote_reply(answer).unwrap_or_default()
      }
      RaftMessage::VoteReply(answer) => raft.on_vote_reply(answer),
      RaftMessage::AppendReply(answer) => raft.on_append_reply(answer),
      _ => {}
    }
    self.finish_election(to, log, was_leader);
    self.retain(to);
    if let Some(reply) = reply {
      self.send(log, to, flight.from, reply);
    }
    for (peer, vote) in self.others(to).into_iter().zip(votes) {
      self.send(log, to, peer, RaftMessage::RequestVote(vote));
    }
    self.apply(to);
  }

  fn run(mut self) -> Outcome {
    let ids: Vec<HostId> = self.nodes.keys().copied().collect();
    for id in ids {
      let phase = self.draw(HEARTBEAT_NS);
      self.schedule(phase, Event::Tick(id));
    }
    while let Some(Reverse((at, _, slot))) = self.queue.pop() {
      if at >= self.shape.duration_ns {
        break;
      }
      self.now = at;
      self.apply_crash();
      match self.events[slot].take() {
        Some(Event::Tick(id)) => self.tick(id),
        Some(Event::Deliver(flight)) => self.deliver(flight),
        Some(Event::ProbeAck { at, peer, sent_ns }) if !self.down(at) => {
          let round_trip = self.now.saturating_sub(sent_ns);
          self
            .nodes
            .get_mut(&at)
            .unwrap()
            .paths
            .entry(peer)
            .or_default()
            .on_sample(round_trip);
        }
        Some(Event::ProbeAck { .. }) | None => {}
      }
    }
    let _ = &self.hosts;
    self.outcome
  }
}

/// The median and 99th percentile of `values`, in milliseconds (zeros when empty).
fn percentiles(values: &mut [u64]) -> (u64, u64) {
  values.sort_unstable();
  let at = |p: usize| {
    values
      .get(p * (values.len().max(1) - 1) / 100)
      .copied()
      .unwrap_or(0)
      / MS
  };
  (at(50), at(99))
}

/// What a set of seeds measured for one shape: per stream, the median over seeds of each seed's median and
/// 99th percentile latency and its longest gap (the keyed stream's also for the watched log's commands
/// alone), and the messages sent in all.
#[derive(Debug, PartialEq, Eq)]
struct Measured {
  keyed_ms: (u64, u64),
  global_ms: (u64, u64),
  keyed_gap_ms: u64,
  watched_keyed_gap_ms: u64,
  global_gap_ms: u64,
  messages: u64,
}

fn median(values: &mut [u64]) -> u64 {
  values.sort_unstable();
  values[values.len() / 2]
}

fn measure(seeds: u64, logs: usize, duration_ns: u64, crash: Option<Crash>) -> Measured {
  let (mut keyed, mut global, mut keyed_gap, mut watched_keyed_gap, mut global_gap) =
    (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
  let mut messages = 0;
  for seed in 0..seeds {
    let mut outcome = Sim::new(Shape {
      logs,
      seed,
      duration_ns,
      crash,
    })
    .run();
    keyed.push(percentiles(&mut outcome.keyed_ns));
    global.push(percentiles(&mut outcome.global_ns));
    keyed_gap.push(outcome.keyed_gap_ns / MS);
    watched_keyed_gap.push(outcome.watched_keyed_gap_ns / MS);
    global_gap.push(outcome.global_gap_ns / MS);
    messages += outcome.messages;
  }
  let pick = |pairs: &[(u64, u64)], which: fn(&(u64, u64)) -> u64| {
    let mut values: Vec<u64> = pairs.iter().map(which).collect();
    median(&mut values)
  };
  Measured {
    keyed_ms: (pick(&keyed, |pair| pair.0), pick(&keyed, |pair| pair.1)),
    global_ms: (pick(&global, |pair| pair.0), pick(&global, |pair| pair.1)),
    keyed_gap_ms: median(&mut keyed_gap),
    watched_keyed_gap_ms: median(&mut watched_keyed_gap),
    global_gap_ms: median(&mut global_gap),
    messages,
  }
}

/// Shape: the seeds and the run CI's gate takes: the first elections, a crash of a log's preferred voter
/// from 20 s to 30 s, and 5 s after.
const GATE_SEEDS: u64 = 2;
const GATE_DURATION_NS: u64 = 35_000 * MS;
const GATE_CRASH_NS: (u64, u64) = (20_000 * MS, 30_000 * MS);

/// What `logs` logs measured, steady and with each log's preferred voter crashed over `crash_ns` in turn, and
/// what a keyed command expects to wait — in milliseconds, times the regions — when it is proposed as one of
/// the regions is lost, each alike. Its log's leader was in the lost region in one case of `regions`, and it
/// waits out the pause that crash leaves its log's keyed commands; otherwise it commits at the steady
/// median. Averaged over the logs (its key picks one alike): the mean pause plus `regions - 1` medians.
fn keyed_expectation(
  seeds: u64,
  logs: usize,
  duration_ns: u64,
  crash_ns: (u64, u64),
) -> (u64, Measured, Vec<Measured>) {
  let steady = measure(seeds, logs, duration_ns, None);
  let crashed: Vec<Measured> = (0..logs)
    .map(|log| {
      let crash = Crash {
        from_ns: crash_ns.0,
        until_ns: crash_ns.1,
        log,
      };
      measure(seeds, logs, duration_ns, Some(crash))
    })
    .collect();
  let regions = u64::try_from(REGIONS.len()).unwrap();
  let pauses: u64 = crashed.iter().map(|run| run.watched_keyed_gap_ms).sum();
  let expectation = pauses / u64::try_from(logs).unwrap() + (regions - 1) * steady.keyed_ms.0;
  (expectation, steady, crashed)
}

/// §3.6's trade, as CI's gate, on the five regions (measured 2026-09-29 over 20 seeds, `docs/wip/BENCHMARKS.md`):
/// with five logs led apart, a crash of log 0's leader pauses the keyed stream about half a second where one
/// log pauses it for its election (463 ms against 4,484 ms); global commands pay for it, waiting for every
/// log's barrier (an 839 ms median against 199 ms), and so does every period, five logs' messages. A crash of
/// any other log's leader stalls every log's keyed commands (3,713–6,312 ms): the global commands log 0 goes
/// on committing wait for the lost log's barrier, and the keyed commands behind every other log's barriers
/// wait for them. And a keyed command gains nothing in expectation: a log that lost its leader pauses its
/// keyed commands no less, and it is lost as often — every region leads one log where one region led the
/// one — while every other command commits more slowly (1,301 ms against 1,036 ms). So the groups keep one
/// log: the council's commands are all global, and the root group's region promotion is such a keyed command
/// (research record §3.6).
#[test]
fn spread_logs_keep_keyed_commands_flowing_and_global_ones_pay_for_it() {
  let (one_expected, one, one_crashed) =
    keyed_expectation(GATE_SEEDS, 1, GATE_DURATION_NS, GATE_CRASH_NS);
  let (five_expected, five, five_crashed) =
    keyed_expectation(GATE_SEEDS, 5, GATE_DURATION_NS, GATE_CRASH_NS);
  eprintln!("one log: steady {one:?}, crashed {one_crashed:?}, expected {one_expected}");
  eprintln!("five logs: steady {five:?}, crashed {five_crashed:?}, expected {five_expected}");
  assert!(
    five_crashed[0].keyed_gap_ms * 4 < one_crashed[0].keyed_gap_ms,
    "keyed commands kept flowing"
  );
  for (log, run) in five_crashed.iter().enumerate().skip(1) {
    assert!(
      run.keyed_gap_ms > five_crashed[0].keyed_gap_ms * 4,
      "log {log}'s lost leader stalled every log's keyed commands: {run:?}"
    );
  }
  assert!(
    five.global_ms.0 > one.global_ms.0 * 2,
    "global commands waited for the barriers"
  );
  assert!(five.messages > one.messages * 4, "five logs' messages");
  assert!(
    five_expected >= one_expected,
    "five logs gained a keyed command nothing in expectation"
  );
}

/// A measurement tool: one, two, three and five logs over the five regions, steady and with each log's
/// preferred voter crashed for twenty seconds in turn, printed with a keyed command's expectation as a region
/// is lost (`docs/wip/BENCHMARKS.md`). `SLATES_MULTILOG_SEEDS=20 cargo test -p slates-cluster --release --test
/// multilog_timed -- --ignored --exact multi_log_on_the_failure_path --nocapture`. Skips, saying so, without
/// the variable.
#[test]
#[ignore = "a measurement tool, run by hand with its environment set"]
fn multi_log_on_the_failure_path() {
  let Some(seeds) = std::env::var("SLATES_MULTILOG_SEEDS")
    .ok()
    .and_then(|seeds| seeds.parse::<u64>().ok())
  else {
    eprintln!("skipping: set SLATES_MULTILOG_SEEDS to measure");
    return;
  };
  let regions = u64::try_from(REGIONS.len()).unwrap();
  for logs in [1, 2, 3, 5] {
    let (expectation, steady, crashed) =
      keyed_expectation(seeds, logs, 70_000 * MS, (30_000 * MS, 50_000 * MS));
    eprintln!("{logs} logs: steady {steady:?}");
    for (log, run) in crashed.iter().enumerate() {
      eprintln!("  log {log}'s voter crashed for 20 s: {run:?}");
    }
    eprintln!(
      "  a keyed command's expectation as a region is lost: {} ms",
      expectation / regions
    );
  }
}
