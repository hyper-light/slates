//! The configuration council's timing law proven on a **WAN profile**, over the simulated fabric's latency
//! model (§4.8 "Derived constants": *"election timeout for the configuration group ≥ 10 × broadcast RTT
//! p99 with the randomization span from RTT variance"*; `docs/wip/wan-timeout.md`). Three voters, each a
//! node running what the daemon's control shard runs: a probe loop to each peer at the heartbeat cadence
//! whose acknowledged round trips feed the node's per-peer path estimate (`timing::PathRtt`, as the
//! daemon's SWIM probe does), and one coordinator loop driving its `RegionalCouncil` the way
//! `server/src/fleet.rs::drive_config_council` does — a leader replicates to every voter each period and
//! judges its quorum on the election cadence; a follower ages the shared `ElectionTimer` and campaigns
//! through the pre-vote then vote rounds over the shared `broadcast`, folding late replies at the next
//! period's settle and dropping late pre-vote replies, exactly as the daemon's `Dispatch` does.
//!
//! The experiment's input is the **rule**: `FIXED` is the daemon as it stood before 2026-09-14 (the
//! ten-period floor timing and a one-period round budget) and `DERIVED` is the timing law and the round
//! budget from the measured tail; the profiles are the fabric's. Both rules run the same harness code, so
//! a difference in outcome is the rule's. Every outcome is on the virtual clock and replays from its
//! seed; the box's load does not bear on it. Test by use (R5).

// Test harness: an unwrap, expect or panic here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use rustix::net::{Ipv4Addr, SocketAddrV4};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use slates_cluster::config_group::RegionalCouncil;
use slates_cluster::raft_wire::RaftMessage;
use slates_cluster::timing::{ElectionTimer, ElectionTiming, PathRtt, RoundAnchors, round_budget};
use slates_cluster::{CommitBudget, Stragglers, TimedReply, broadcast, request_within};
use slates_db::register::{HostId, Quorum};
use slates_rt::futures::{now_ns, sleep};
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::{SimDelay, SimRuntime, sim_udp_set_delay};
use slates_rt::udp::UdpSocket;
use slates_transport::endpoint::{Endpoint, MIN_DATAGRAM_BYTES};
use slates_transport::handshake::Identity;
use slates_transport::rtt::RttEstimator;

const NAME: &str = "slates-fleet";
/// Shape: the fleet's frame class — a whole council message or probe in one frame, as the daemon's cap
/// derived from the RFC 9000 §14.1 minimum datagram gives it, so an exchange is one round trip.
const FRAME_CAP: usize = MIN_DATAGRAM_BYTES;
/// Shape: the daemon's coordinator period, `slates_server::daemon::HEARTBEAT_NS` (100 ms) — mirrored here
/// so the harness's periods are the daemon's.
const HEARTBEAT_NS: u64 = 100_000_000;
/// The daemon's round-budget anchors (`fleet::consensus_budget`): a tenth of a period per poll, the SWIM
/// suspicion span of two periods as the stall window, and the last quarter of the deadline as the
/// lookahead — the same numbers, so the harness's budget is the daemon's at every tail.
const ANCHORS: RoundAnchors = RoundAnchors {
  heartbeat_ns: HEARTBEAT_NS,
  stall_periods: 2,
  polls_per_period: 10,
  lookahead: (3, 4),
};
/// The three voters — the smallest council whose majority is agreed, not self-elected.
const VOTERS: [HostId; 3] = [HostId(1), HostId(2), HostId(3)];
/// The candidate floor at f=1 (2f+1), the scatter the council's neighbourhoods are bounded to.
const SCATTER: u64 = 3;
/// Format: the stream kind a probe rides (a fixed label, as the daemon's stream ids are).
const PROBE_KIND: u64 = 2;
/// Format: the stream kind a council message rides.
const COUNCIL_KIND: u64 = 7;
/// Shape: how many establish attempts a session is given before the harness gives up on it — each attempt
/// is the transport's own bounded handshake; a healthy fabric needs one.
const ESTABLISH_ATTEMPTS: u32 = 4;

/// Shape: the inter-region path — 80 ms one way ± 20 ms: Japan East → East US, published at a 162 ms P50
/// round trip over the 30 days ending 2026-07-30 (Microsoft's "Azure network round-trip latency
/// statistics"; the other pairs it stands beside are in `docs/wip/wan-timeout.md`).
const INTER_REGION: SimDelay = SimDelay::in_order(80_000_000, 20_000_000);
/// Shape: a geostationary-satellite class path — 500 ms one way ± 100 ms (two GEO hops; a single hop is
/// ~250 ms one way from the 35,786 km orbit at the speed of light) — the profile at which a one-second
/// election timeout is inside the leader's round, so the fixed timing campaigns against a live leader.
const GEO_CLASS: SimDelay = SimDelay::in_order(500_000_000, 100_000_000);

/// The rule under test — the experiment's independent variable, applied to the same harness code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rule {
  /// The election timing: the derived law, or the ten-period floor the daemon ran before.
  derived_timing: bool,
  /// The round budget: derived from the measured tail, or the one-period base the daemon ran before.
  derived_budget: bool,
}

/// The daemon as it stood before 2026-09-14.
const FIXED: Rule = Rule {
  derived_timing: false,
  derived_budget: false,
};
/// The daemon with this change.
const DERIVED: Rule = Rule {
  derived_timing: true,
  derived_budget: true,
};
/// The timeout rule alone, isolated: the fixed floor timing over the derived round budget, so what the
/// timing law adds is told apart from what the budget adds.
const FIXED_TIMING_DERIVED_BUDGET: Rule = Rule {
  derived_timing: false,
  derived_budget: true,
};

/// One node's state — what the daemon keeps in its control shard's `ShardState` for the council: the
/// council itself, the measured paths, the election timer, the record sessions the coordinator borrows,
/// and the rounds in flight whose stragglers it settles. Held in a thread-local map, as the daemon's shard
/// state is, and only ever borrowed briefly, never across an await.
struct Node {
  council: RegionalCouncil,
  paths: BTreeMap<HostId, PathRtt>,
  timer: ElectionTimer,
  record_sessions: BTreeMap<HostId, Option<Endpoint>>,
  in_flight: Vec<Pending>,
  killed_at: Option<u64>,
  campaigns: u64,
  leading_since: Option<u64>,
  leader_spans: Vec<(u64, u64)>,
  timing: ElectionTiming,
}

impl Node {
  fn new(host: HostId) -> Node {
    Node {
      council: RegionalCouncil::new(
        host,
        VOTERS.to_vec(),
        VOTERS.to_vec(),
        Quorum { f: 1 },
        BTreeMap::new(),
        SCATTER,
        false,
      ),
      paths: BTreeMap::new(),
      timer: ElectionTimer::new(),
      record_sessions: BTreeMap::new(),
      in_flight: Vec::new(),
      killed_at: None,
      campaigns: 0,
      leading_since: None,
      leader_spans: Vec::new(),
      timing: ElectionTiming::floor(),
    }
  }
}

/// A round whose stragglers are still out — the harness's form of the daemon's `Dispatch`: the voters
/// still outstanding on it and the reply channel. A late reply is folded as the daemon's `late_raft_reply`
/// folds it: a council vote or append reply counts; a late pre-vote reply is dropped, the next campaign
/// re-runs its pre-vote.
struct Pending {
  sent: Vec<HostId>,
  stragglers: Stragglers,
}

thread_local! {
  static NODES: RefCell<BTreeMap<HostId, Node>> = const { RefCell::new(BTreeMap::new()) };
  /// When the scenario ends (virtual nanoseconds); every loop leaves at or after it.
  static STOP_AT_NS: Cell<u64> = const { Cell::new(0) };
  /// When the current leader is to die, if the scenario kills one; taken by the node that dies.
  static KILL_LEADER_AT_NS: Cell<Option<u64>> = const { Cell::new(None) };
  /// Every time a node became leader: (virtual time, node) — the fleet-wide leadership history.
  static LEADER_EVENTS: RefCell<Vec<(u64, HostId)>> = const { RefCell::new(Vec::new()) };
  /// Every time a follower's timer fired and it campaigned (a pre-election began): (virtual time, node).
  static CAMPAIGN_EVENTS: RefCell<Vec<(u64, HostId)>> = const { RefCell::new(Vec::new()) };
}

fn with_node<R>(host: HostId, f: impl FnOnce(&mut Node) -> R) -> R {
  NODES.with(|nodes| {
    f(nodes
      .borrow_mut()
      .get_mut(&host)
      .expect("a node of the fleet"))
  })
}

fn stopped() -> bool {
  now_ns() >= STOP_AT_NS.with(Cell::get)
}

fn killed(host: HostId) -> bool {
  with_node(host, |n| n.killed_at.is_some())
}

fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 1,
    tasks_per_shard: 256,
    timers_per_shard: 256,
    ring_entries: 256,
    step_budget_ns: 2_000_000_000,
    timer_tick_ns: 100_000,
    batch: 64,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
    wake_tracking: None,
  }
}

fn self_signed(name: &str) -> Identity {
  let key = rcgen::KeyPair::generate().unwrap();
  let cert = rcgen::CertificateParams::new(vec![name.to_owned()])
    .unwrap()
    .self_signed(&key)
    .unwrap();
  Identity::from_der(
    cert.der().clone(),
    PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
  )
}

fn bind() -> UdpSocket {
  UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap()
}

/// Establishes `endpoint`, retrying the transport's bounded handshake a few times; a session that never
/// establishes is a harness failure.
async fn establish(mut endpoint: Endpoint, what: &str) -> Endpoint {
  for _ in 0..ESTABLISH_ATTEMPTS {
    if endpoint.establish().await.is_ok() {
      return endpoint;
    }
  }
  panic!("{what}: the session never established");
}

/// Races `future` against the virtual clock reaching `until_ns`; `None` if the clock won.
async fn until<F: std::future::Future>(until_ns: u64, future: F) -> Option<F::Output> {
  let mut future = std::pin::pin!(future);
  let mut deadline = std::pin::pin!(sleep(until_ns.saturating_sub(now_ns()).max(1)));
  std::future::poll_fn(|cx| {
    if let std::task::Poll::Ready(out) = std::future::Future::poll(future.as_mut(), cx) {
      return std::task::Poll::Ready(Some(out));
    }
    if std::future::Future::poll(deadline.as_mut(), cx).is_ready() {
      return std::task::Poll::Ready(None);
    }
    std::task::Poll::Pending
  })
  .await
}

/// The serve side of one session at `owner`, answering `peer` until the scenario stops or `owner` dies:
/// the probe plane acknowledges each ping; the council plane answers each message through the council,
/// as the daemon's `serve_council` does. Each serve is raced against the next heartbeat so a death is
/// honoured within a period — a dead node answers nothing.
async fn serve_session(owner: HostId, mut endpoint: Endpoint, council: bool) {
  endpoint = establish(endpoint, "serve").await;
  loop {
    if stopped() || killed(owner) {
      return;
    }
    let next_check = now_ns().saturating_add(HEARTBEAT_NS);
    let served = until(
      next_check,
      endpoint.serve_once(|_, request| {
        if council {
          with_node(owner, |n| match RaftMessage::decode(&request) {
            Ok(message) => n
              .council
              .answer(message)
              .map(|reply| reply.encode())
              .unwrap_or_default(),
            Err(_) => Vec::new(),
          })
        } else {
          b"ack".to_vec()
        }
      }),
    )
    .await;
    if let Some(Err(_)) = served {
      return;
    }
  }
}

/// The probe side of one path at `owner` to `peer`: a ping every heartbeat, its deadline the path's
/// measured tail floored at the heartbeat (the daemon's `ProbeTiming`, before any sample the transport's
/// initial probe timeout), its acknowledged round trip folded into the path estimate.
async fn probe_peer(owner: HostId, peer: HostId, endpoint: Endpoint) {
  let mut endpoint = establish(endpoint, "probe").await;
  let initial = RttEstimator::new().initial_pto();
  loop {
    if stopped() || killed(owner) {
      return;
    }
    let deadline = with_node(owner, |n| {
      n.paths
        .get(&peer)
        .and_then(PathRtt::tail_ns)
        .unwrap_or(initial)
        .max(HEARTBEAT_NS)
    });
    let (reply, returned) = request_within(endpoint, PROBE_KIND, b"ping", deadline).await;
    endpoint = returned;
    if let Some(round_trip_ns) = reply.round_trip_ns {
      with_node(owner, |n| {
        n.paths.entry(peer).or_default().on_sample(round_trip_ns);
      });
    }
    sleep(HEARTBEAT_NS).await;
  }
}

fn take_sessions(owner: HostId, others: &[HostId]) -> Vec<(HostId, Endpoint)> {
  with_node(owner, |n| {
    others
      .iter()
      .filter_map(|host| {
        n.record_sessions
          .get_mut(host)
          .and_then(Option::take)
          .map(|endpoint| (*host, endpoint))
      })
      .collect()
  })
}

fn return_sessions(owner: HostId, sessions: Vec<(HostId, Endpoint)>) {
  with_node(owner, |n| {
    for (host, endpoint) in sessions {
      n.record_sessions.insert(host, Some(endpoint));
    }
  });
}

/// Folds a round's replies into `owner`'s council and path estimates: every reply with a round trip
/// samples the path to its voter (Karn: a timed-out one does not); a vote or append reply is folded; a
/// pre-vote reply is folded only when `fold_pre_votes` (the round that asked for it) and the first
/// follow-on request it yields — the real vote — is returned.
fn fold_replies(
  owner: HostId,
  replies: &[(HostId, TimedReply)],
  fold_pre_votes: bool,
) -> Option<RaftMessage> {
  with_node(owner, |n| {
    let mut follow_on = None;
    for (host, reply) in replies {
      if let Some(round_trip_ns) = reply.round_trip_ns {
        n.paths.entry(*host).or_default().on_sample(round_trip_ns);
      }
      match RaftMessage::decode(&reply.bytes) {
        Ok(RaftMessage::PreVoteReply(_)) if !fold_pre_votes => {}
        Ok(message) => {
          if let Some(request) = n.council.fold_reply(message).into_iter().next() {
            follow_on = Some(request);
          }
        }
        Err(_) => {}
      }
    }
    follow_on
  })
}

/// Settles `owner`'s rounds in flight at the top of a period, as the daemon's coordinator settles its
/// dispatches: each straggler's late reply is folded (its round trip sampled; a late pre-vote reply is
/// dropped) and its session returned; a round whose stragglers are all accounted for is dropped, any voter
/// still outstanding on it marked lost.
fn settle(owner: HostId) {
  let mut pending = with_node(owner, |n| std::mem::take(&mut n.in_flight));
  let mut kept = Vec::new();
  for mut round in pending.drain(..) {
    let (arrived, done) = round.stragglers.recover_replies();
    let replies: Vec<(HostId, TimedReply)> = arrived
      .iter()
      .map(|(host, reply, _)| (*host, reply.clone()))
      .collect();
    let _ = fold_replies(owner, &replies, false);
    let returned: Vec<(HostId, Endpoint)> = arrived
      .into_iter()
      .map(|(host, _, endpoint)| (host, endpoint))
      .collect();
    round
      .sent
      .retain(|host| !returned.iter().any(|(back, _)| back == host));
    return_sessions(owner, returned);
    if done {
      with_node(owner, |n| {
        for host in &round.sent {
          n.record_sessions.remove(host);
        }
      });
    } else {
      kept.push(round);
    }
  }
  with_node(owner, |n| n.in_flight = kept);
}

/// One replication round as leader (`drive_council_replication`): the append each borrowed voter is owed,
/// shipped concurrently, the timely replies folded, the round left in flight for its stragglers.
async fn drive_replication(owner: HostId, others: &[HostId], budget: CommitBudget) {
  let sessions = take_sessions(owner, others);
  if sessions.is_empty() {
    return;
  }
  let appends: BTreeMap<HostId, Vec<u8>> = with_node(owner, |n| {
    sessions
      .iter()
      .filter_map(|(host, _)| {
        n.council
          .replication_for(*host)
          .map(|append| (*host, RaftMessage::AppendEntries(append).encode()))
      })
      .collect()
  });
  let mut requests = Vec::new();
  let mut kept = Vec::new();
  for (host, endpoint) in sessions {
    match appends.get(&host) {
      Some(bytes) => requests.push((host, bytes.clone(), endpoint)),
      None => kept.push((host, endpoint)),
    }
  }
  let sent: Vec<HostId> = requests.iter().map(|(host, _, _)| *host).collect();
  let (replied, stragglers) = broadcast(requests, COUNCIL_KIND, budget).await;
  let mut recovered = kept;
  let mut replies = Vec::with_capacity(replied.len());
  for (host, reply, endpoint) in replied {
    replies.push((host, reply));
    recovered.push((host, endpoint));
  }
  let _ = fold_replies(owner, &replies, false);
  let outstanding: Vec<HostId> = sent
    .into_iter()
    .filter(|host| !recovered.iter().any(|(back, _)| back == host))
    .collect();
  with_node(owner, |n| {
    n.in_flight.push(Pending {
      sent: outstanding,
      stragglers,
    });
  });
  return_sessions(owner, recovered);
}

/// One campaign as a follower whose leader contact lapsed (`drive_council_election`): the pre-vote round
/// over the borrowed voter sessions; on a granted majority the real vote round over the same sessions.
/// Late pre-vote replies are dropped at the settle; late vote replies are folded.
async fn drive_election(owner: HostId, others: &[HostId], budget: CommitBudget) {
  let Some(pre_vote) = with_node(owner, |n| n.council.election_timeout().into_iter().next()) else {
    return;
  };
  let sessions = take_sessions(owner, others);
  if sessions.is_empty() {
    return;
  }
  let pre_bytes = pre_vote.encode();
  let requests: Vec<(HostId, Vec<u8>, Endpoint)> = sessions
    .into_iter()
    .map(|(host, endpoint)| (host, pre_bytes.clone(), endpoint))
    .collect();
  let sent: Vec<HostId> = requests.iter().map(|(host, _, _)| *host).collect();
  let (replied, stragglers) = broadcast(requests, COUNCIL_KIND, budget).await;
  let mut sessions = Vec::with_capacity(replied.len());
  let mut replies = Vec::with_capacity(replied.len());
  for (host, reply, endpoint) in replied {
    replies.push((host, reply));
    sessions.push((host, endpoint));
  }
  let outstanding: Vec<HostId> = sent
    .into_iter()
    .filter(|host| !sessions.iter().any(|(back, _)| back == host))
    .collect();
  with_node(owner, |n| {
    n.in_flight.push(Pending {
      sent: outstanding,
      stragglers,
    });
  });
  let Some(vote) = fold_replies(owner, &replies, true) else {
    return_sessions(owner, sessions);
    return;
  };
  let vote_bytes = vote.encode();
  let requests: Vec<(HostId, Vec<u8>, Endpoint)> = sessions
    .into_iter()
    .map(|(host, endpoint)| (host, vote_bytes.clone(), endpoint))
    .collect();
  let sent: Vec<HostId> = requests.iter().map(|(host, _, _)| *host).collect();
  let (replied, stragglers) = broadcast(requests, COUNCIL_KIND, budget).await;
  let mut recovered = Vec::with_capacity(replied.len());
  let mut replies = Vec::with_capacity(replied.len());
  for (host, reply, endpoint) in replied {
    replies.push((host, reply));
    recovered.push((host, endpoint));
  }
  let _ = fold_replies(owner, &replies, false);
  let outstanding: Vec<HostId> = sent
    .into_iter()
    .filter(|host| !recovered.iter().any(|(back, _)| back == host))
    .collect();
  with_node(owner, |n| {
    n.in_flight.push(Pending {
      sent: outstanding,
      stragglers,
    });
  });
  return_sessions(owner, recovered);
}

/// Records whether `owner` leads this period, keeping the fleet-wide history and the node's spans.
fn observe_leadership(owner: HostId, leads: bool) {
  let now = now_ns();
  with_node(owner, |n| match (n.leading_since, leads) {
    (None, true) => {
      n.leading_since = Some(now);
      LEADER_EVENTS.with(|events| events.borrow_mut().push((now, owner)));
    }
    (Some(since), false) => {
      n.leader_spans.push((since, now));
      n.leading_since = None;
    }
    _ => {}
  });
}

/// This period's view at `owner` under `rule`: whether it leads, its leader-contact count, the other
/// voters, and the timing and round budget derived (or fixed) from the measured paths to them.
struct Period {
  is_leader: bool,
  contact: u64,
  others: Vec<HostId>,
  timing: ElectionTiming,
  budget: CommitBudget,
}

fn derive_period(owner: HostId, rule: Rule) -> Period {
  with_node(owner, |n| {
    let others: Vec<HostId> = n
      .council
      .voters()
      .into_iter()
      .filter(|voter| *voter != owner)
      .collect();
    let paths = others.iter().filter_map(|host| n.paths.get(host));
    let timing = if rule.derived_timing {
      ElectionTiming::derive(HEARTBEAT_NS, paths.clone())
    } else {
      ElectionTiming::floor()
    };
    let tail = rule
      .derived_budget
      .then(|| paths.filter_map(PathRtt::tail_ns).max())
      .flatten();
    n.timing = timing;
    Period {
      is_leader: n.council.is_leader(),
      contact: n.council.leader_contact(),
      others,
      timing,
      budget: round_budget(&ANCHORS, tail),
    }
  })
}

/// Whether `owner`, leading now, is the node the scenario kills at this instant: takes the kill (so no
/// other node dies) and records the death and the end of its leadership.
fn dies_now(owner: HostId, is_leader: bool) -> bool {
  let due = is_leader
    && KILL_LEADER_AT_NS
      .with(Cell::get)
      .is_some_and(|at| now_ns() >= at);
  if !due {
    return false;
  }
  KILL_LEADER_AT_NS.with(|cell| cell.set(None));
  with_node(owner, |n| {
    n.killed_at = Some(now_ns());
    if let Some(since) = n.leading_since.take() {
      n.leader_spans.push((since, now_ns()));
    }
  });
  true
}

/// The coordinator loop of `owner` (`run_record_plane` → `drive_config_council`), under `rule`: settle
/// the rounds in flight, derive this period's timing and budget from the measured paths to the other
/// voters, then lead, self-elect, or age toward a campaign; one heartbeat between periods. The node that
/// leads when the scenario's kill time comes dies here: it stops coordinating, answering and probing.
async fn coordinate(owner: HostId, rule: Rule, council_sessions: Vec<(HostId, Endpoint)>) {
  let mut established = Vec::with_capacity(council_sessions.len());
  for (host, endpoint) in council_sessions {
    established.push((host, establish(endpoint, "council").await));
  }
  return_sessions(owner, established);
  loop {
    if stopped() || killed(owner) {
      return;
    }
    settle(owner);
    let Period {
      is_leader,
      contact,
      others,
      timing,
      budget,
    } = derive_period(owner, rule);
    observe_leadership(owner, is_leader);
    if dies_now(owner, is_leader) {
      return;
    }
    if is_leader {
      drive_replication(owner, &others, budget).await;
      if with_node(owner, |n| n.timer.leader_period(&timing)) {
        with_node(owner, |n| n.council.check_quorum());
      }
    } else {
      follow_or_campaign(owner, contact, &others, &timing, budget).await;
    }
    sleep(HEARTBEAT_NS).await;
  }
}

/// A non-leader's period: the sole voter self-elects; a follower ages its timer and, at its jittered
/// timeout, campaigns and then re-baselines its contact so the campaign's own echoes do not retrigger it.
async fn follow_or_campaign(
  owner: HostId,
  contact: u64,
  others: &[HostId],
  timing: &ElectionTiming,
  budget: CommitBudget,
) {
  if others.is_empty() {
    with_node(owner, |n| {
      let _ = n.council.election_timeout();
      n.timer.reset();
    });
    return;
  }
  if !with_node(owner, |n| n.timer.follower_period(contact, timing, owner)) {
    return;
  }
  with_node(owner, |n| n.campaigns += 1);
  CAMPAIGN_EVENTS.with(|events| events.borrow_mut().push((now_ns(), owner)));
  drive_election(owner, others, budget).await;
  with_node(owner, |n| {
    let contact = n.council.leader_contact();
    n.timer.rebaseline(contact);
  });
}

/// What one node reports at the end of a scenario.
#[derive(Debug)]
struct NodeReport {
  host: HostId,
  campaigns: u64,
  leader_spans: Vec<(u64, u64)>,
  timing: ElectionTiming,
  killed_at: Option<u64>,
  samples: u64,
}

/// What a scenario produced: every node's report, the fleet-wide order in which nodes became leader, and
/// every campaign a follower began.
#[derive(Debug)]
struct Outcome {
  reports: Vec<NodeReport>,
  leader_events: Vec<(u64, HostId)>,
  campaign_events: Vec<(u64, HostId)>,
}

impl Outcome {
  fn report(&self, host: HostId) -> &NodeReport {
    self.reports.iter().find(|r| r.host == host).unwrap()
  }

  fn campaigns(&self) -> u64 {
    self.reports.iter().map(|r| r.campaigns).sum()
  }

  /// Campaigns begun after `since_ns` — with a leader alive throughout, every one is spurious: a follower
  /// presuming a live leader gone.
  fn campaigns_after(&self, since_ns: u64) -> usize {
    self
      .campaign_events
      .iter()
      .filter(|(at, _)| *at > since_ns)
      .count()
  }

  /// One line per node and the histories, for the measurement record (`--nocapture`).
  fn summary(&self, label: &str) {
    eprintln!("== {label}");
    for r in &self.reports {
      eprintln!(
        "  node {} campaigns {} base {} span {} tail {:.1} ms spread {:.1} ms samples {} (timing samples {}) led {:?} killed_at {:?}",
        r.host.0,
        r.campaigns,
        r.timing.base_periods,
        r.timing.span_periods,
        r.timing.broadcast_rtt_tail_ns as f64 / 1e6,
        r.timing.broadcast_rtt_spread_ns as f64 / 1e6,
        r.samples,
        r.timing.samples,
        r.leader_spans
          .iter()
          .map(|(a, b)| (
            *a as f64 / 1e9,
            if *b == u64::MAX {
              f64::INFINITY
            } else {
              *b as f64 / 1e9
            }
          ))
          .collect::<Vec<_>>(),
        r.killed_at.map(|t| t as f64 / 1e9),
      );
    }
    eprintln!(
      "  leader events {:?}",
      self
        .leader_events
        .iter()
        .map(|(t, h)| (*t as f64 / 1e9, h.0))
        .collect::<Vec<_>>()
    );
    eprintln!(
      "  campaign events {:?}",
      self
        .campaign_events
        .iter()
        .map(|(t, h)| (*t as f64 / 1e9, h.0))
        .collect::<Vec<_>>()
    );
  }

  /// The node leading at the end of the scenario, if exactly one was.
  fn leader_at_end(&self) -> Option<HostId> {
    let leading: Vec<HostId> = self
      .reports
      .iter()
      .filter(|r| {
        r.killed_at.is_none()
          && r
            .leader_spans
            .last()
            .is_some_and(|(_, end)| *end == u64::MAX)
      })
      .map(|r| r.host)
      .collect();
    (leading.len() == 1).then(|| leading[0])
  }

  /// Leadership changes after the first leader emerged (or after `since_ns`).
  fn leader_changes_after(&self, since_ns: u64) -> usize {
    self
      .leader_events
      .iter()
      .filter(|(at, _)| *at > since_ns)
      .count()
  }
}

/// Runs one scenario: `voters` in a full mesh over the fabric at `profile`, under `rule`, for `periods`
/// heartbeats of virtual time, killing the current leader at `kill_leader_at_period` if given. Every session
/// of the mesh — a probe and a council session in each direction of each pair — is bound and dialed by one
/// root task that spawns the node loops that own them, as the daemon's membership loop spawns its tasks.
fn run_scenario(
  seed: u64,
  profile: SimDelay,
  rule: Rule,
  periods: u64,
  kill_leader_at_period: Option<u64>,
) -> Outcome {
  let mut sim = SimRuntime::new(&config(), seed).unwrap();
  sim_udp_set_delay(profile);
  NODES.with(|nodes| {
    let mut nodes = nodes.borrow_mut();
    nodes.clear();
    for host in VOTERS {
      nodes.insert(host, Node::new(host));
    }
  });
  STOP_AT_NS.with(|cell| cell.set(periods.saturating_mul(HEARTBEAT_NS)));
  KILL_LEADER_AT_NS.with(|cell| cell.set(kill_leader_at_period.map(|p| p * HEARTBEAT_NS)));
  LEADER_EVENTS.with(|events| events.borrow_mut().clear());
  CAMPAIGN_EVENTS.with(|events| events.borrow_mut().clear());
  let shard = sim.shard_ids()[0];

  sim
    .spawn_on(shard, async move {
      let identities: BTreeMap<HostId, Identity> = VOTERS
        .iter()
        .map(|host| (*host, self_signed(NAME)))
        .collect();
      let certificates: BTreeMap<HostId, CertificateDer<'static>> = identities
        .iter()
        .map(|(host, identity)| (*host, identity.certificate()))
        .collect();
      let mut council_sessions: BTreeMap<HostId, Vec<(HostId, Endpoint)>> = BTreeMap::new();
      for dialer in VOTERS {
        for peer in VOTERS {
          if dialer == peer {
            continue;
          }
          for council in [false, true] {
            // The dialer's socket and the peer's serve socket for this directed session; each end is
            // told the other's address, as the daemon's manifest tells every node its peers'.
            let dial_socket = bind();
            let serve_socket = bind();
            let dial_addr = dial_socket.local_addr().unwrap();
            let serve_addr = serve_socket.local_addr().unwrap();
            let server = Endpoint::server(
              serve_socket,
              dial_addr,
              &identities[&peer],
              std::slice::from_ref(&certificates[&dialer]),
              FRAME_CAP,
            )
            .unwrap();
            let client = Endpoint::client(
              dial_socket,
              serve_addr,
              &identities[&dialer],
              &certificates[&peer],
              NAME,
              FRAME_CAP,
            )
            .unwrap();
            let serve = slates_rt::futures::spawn(serve_session(peer, server, council)).unwrap();
            let _ = slates_rt::futures::detach(serve);
            if council {
              council_sessions
                .entry(dialer)
                .or_default()
                .push((peer, client));
            } else {
              let probe = slates_rt::futures::spawn(probe_peer(dialer, peer, client)).unwrap();
              let _ = slates_rt::futures::detach(probe);
            }
          }
        }
      }
      for (owner, sessions) in council_sessions {
        let coordinator = slates_rt::futures::spawn(coordinate(owner, rule, sessions)).unwrap();
        let _ = slates_rt::futures::detach(coordinator);
      }
    })
    .unwrap();

  sim.run_until_idle();

  let reports = NODES.with(|nodes| {
    nodes
      .borrow()
      .iter()
      .map(|(host, n)| {
        // A node still leading at the stop has an open span; it is reported as open.
        let mut spans = n.leader_spans.clone();
        if let Some(since) = n.leading_since {
          spans.push((since, u64::MAX));
        }
        NodeReport {
          host: *host,
          campaigns: n.campaigns,
          leader_spans: spans,
          timing: n.timing,
          killed_at: n.killed_at,
          samples: n.paths.values().map(PathRtt::samples).sum(),
        }
      })
      .collect::<Vec<_>>()
  });
  Outcome {
    reports,
    leader_events: LEADER_EVENTS.with(|events| events.borrow().clone()),
    campaign_events: CAMPAIGN_EVENTS.with(|events| events.borrow().clone()),
  }
}

/// Shape: the periods a scenario runs — three hundred heartbeats, thirty seconds of virtual time: enough
/// for the derived timing at the inter-region profile (a two-second base) to elect and then hold a leader
/// through many timeouts' worth of periods, and for the fixed rule's failure to be a settled fact.
const PERIODS: u64 = 300;
/// Shape: the period at which the leader is killed in the death scenario — after the derived timing has
/// converged and a leader has held for a hundred periods.
const KILL_AT_PERIOD: u64 = 150;

/// AC (§4.8 "Derived constants"; R8): at the LAN profile — every round trip inside one heartbeat — the
/// derived rule **is** the fixed rule: the timing at every node sits at the floor while having been
/// measured (samples counted), the round budget is the loopback's, and the fleet's leadership history is
/// the same to the nanosecond under both rules. The laptop and LAN fleet are unchanged by construction.
#[test]
fn at_the_lan_profile_the_derived_rule_is_the_fixed_rule_by_history() {
  let fixed = run_scenario(11, SimDelay::NONE, FIXED, PERIODS, None);
  fixed.summary("LAN, fixed rule");
  let derived = run_scenario(11, SimDelay::NONE, DERIVED, PERIODS, None);
  derived.summary("LAN, derived rule");
  assert!(
    !fixed.leader_events.is_empty(),
    "the LAN council elected a leader under the fixed rule: {fixed:?}"
  );
  assert_eq!(
    fixed.leader_events, derived.leader_events,
    "one leadership history under both rules at the LAN profile"
  );
  for report in &derived.reports {
    assert_eq!(
      report.timing.base_periods,
      ElectionTiming::floor().base_periods,
      "{report:?}: the derived base is the floor on a LAN"
    );
    assert_eq!(
      report.timing.span_periods,
      ElectionTiming::floor().span_periods,
      "{report:?}: the derived span is the floor on a LAN"
    );
    assert!(
      report.samples > 0 && report.timing.samples > 0,
      "{report:?}: the floor was measured, not defaulted"
    );
    assert!(
      report.timing.broadcast_rtt_tail_ns < HEARTBEAT_NS,
      "{report:?}: every LAN round trip is inside a heartbeat"
    );
  }
  assert_eq!(
    derived.leader_changes_after(derived.leader_events[0].0),
    0,
    "one leader held for the whole run: {:?}",
    derived.leader_events
  );
}

/// AC (§4.8 "Derived constants", the WAN case): at the inter-region profile (80 ms ± 20 ms one way) the
/// daemon's fixed rule **never elects a leader** — every pre-vote round expires at three quarters of a
/// period with its replies still in flight, and a late pre-vote reply is dropped by design — while the
/// derived rule elects one and holds it for the rest of the run, its base at ten times the measured tail
/// (about two seconds, twenty periods and more) rather than the one-second floor. The fixed rule's
/// campaigns are counted so its failure is a measured fact, not an absence.
#[test]
fn at_the_inter_region_profile_the_fixed_rule_never_elects_and_the_derived_rule_holds_one_leader() {
  let fixed = run_scenario(11, INTER_REGION, FIXED, PERIODS, None);
  fixed.summary("inter-region, fixed rule");
  assert!(
    fixed.leader_events.is_empty(),
    "the fixed rule elected across an inter-region path: {:?}",
    fixed.leader_events
  );
  assert!(
    fixed.campaigns() > 0,
    "the fixed rule campaigned and failed, not merely idled: {fixed:?}"
  );

  let derived = run_scenario(11, INTER_REGION, DERIVED, PERIODS, None);
  derived.summary("inter-region, derived rule");
  assert_first_leader_held_with_no_campaign(&derived);
  assert_timing_is_above_the_floor(&derived, INTER_REGION);
}

/// Every node's derived timing on a far path: the base above the floor, and the measured tail bounding
/// the profile's round trip.
fn assert_timing_is_above_the_floor(outcome: &Outcome, profile: SimDelay) {
  for report in &outcome.reports {
    assert!(
      report.timing.base_periods > ElectionTiming::floor().base_periods,
      "{report:?}: the derived base is above the floor on a WAN"
    );
    assert!(
      report.timing.broadcast_rtt_tail_ns > 2 * profile.one_way_ns(),
      "{report:?}: the tail bounds the path's round trip"
    );
  }
}

/// AC (§4.8 "Derived constants"; Raft §5.2): at the inter-region profile, under the derived rule, a leader
/// that dies is replaced within the derived election budget — the base plus the randomization span, with
/// one retry allowed for a split vote — and the replacement then holds.
#[test]
fn at_the_inter_region_profile_a_dead_leader_is_replaced_within_the_derived_budget() {
  let outcome = run_scenario(11, INTER_REGION, DERIVED, PERIODS, Some(KILL_AT_PERIOD));
  outcome.summary("inter-region, derived rule, the leader killed");
  let killed: Vec<&NodeReport> = outcome
    .reports
    .iter()
    .filter(|r| r.killed_at.is_some())
    .collect();
  assert_eq!(
    killed.len(),
    1,
    "exactly one node — the leader — died: {outcome:?}"
  );
  let killed_at = killed[0].killed_at.unwrap();
  let replacement = outcome
    .leader_events
    .iter()
    .find(|(at, host)| *at > killed_at && *host != killed[0].host)
    .copied();
  let Some((elected_at, successor)) = replacement else {
    panic!("no successor was elected after the leader died: {outcome:?}");
  };
  let survivor_timing = outcome.report(successor).timing;
  let budget_ns =
    2 * u64::from(survivor_timing.base_periods + survivor_timing.span_periods) * HEARTBEAT_NS;
  assert!(
    elected_at - killed_at <= budget_ns,
    "the successor was elected {} ns after the death, within twice the derived base plus span ({budget_ns} ns): {outcome:?}",
    elected_at - killed_at
  );
  assert_eq!(
    outcome.leader_changes_after(elected_at),
    0,
    "the successor held for the rest of the run: {:?}",
    outcome.leader_events
  );
}

/// AC (§4.8 "Derived constants" — the non-vacuity contrast for the timeout rule itself): at the
/// geostationary-class profile, with the round budget derived for both, the fixed ten-period timing
/// **campaigns against a live leader** — the leader's heartbeat, one round trip plus a period apart, lands
/// past a follower's one-to-two-second timeout, so that follower presumes it gone and begins a
/// pre-election — while the derived timing (ten times the measured tail) begins none once the first
/// leader is elected. Measured (2026-09-14, 1,800 periods): the leader is **not** deposed under either
/// rule — Raft's PreVote (§9.6) refuses the timed-out follower's pre-vote at the follower that still
/// hears the leader, and the jitter rotation lengthens the campaigner's next timeout past the gap — so the
/// timeout rule's cost at a WAN profile is spurious campaigns (a round each), not lost leadership; the
/// WAN-blocking defect was the round budget (the inter-region test above). Campaigns after the first
/// election, with every node alive, are the measure.
#[test]
fn at_the_geo_class_profile_the_fixed_timing_campaigns_against_a_live_leader_and_the_derived_timing_does_not()
 {
  let periods = 6 * PERIODS;
  let fixed_timing = run_scenario(11, GEO_CLASS, FIXED_TIMING_DERIVED_BUDGET, periods, None);
  fixed_timing.summary("GEO class, fixed timing over the derived budget");
  assert!(
    !fixed_timing.leader_events.is_empty(),
    "the fixed timing over the derived budget elected at least once: {fixed_timing:?}"
  );
  let first = fixed_timing.leader_events[0].0;
  let spurious = fixed_timing.campaigns_after(first);
  assert!(
    spurious > 0,
    "the fixed timing campaigned against a live leader at least once: {:?}",
    fixed_timing.campaign_events
  );

  let derived = run_scenario(11, GEO_CLASS, DERIVED, periods, None);
  derived.summary("GEO class, derived timing over the derived budget");
  assert_first_leader_held_with_no_campaign(&derived);
}

/// The derived rule's steady state, asserted after any profile's run: a leader was elected, no campaign
/// began after it, it was never replaced, and every node's timeout is at least ten times its measured
/// tail.
fn assert_first_leader_held_with_no_campaign(outcome: &Outcome) {
  assert!(
    !outcome.leader_events.is_empty(),
    "the derived rule elected a leader: {outcome:?}"
  );
  let first = outcome.leader_events[0].0;
  assert_eq!(
    outcome.campaigns_after(first),
    0,
    "the derived timing began no campaign while its leader lived: {:?}",
    outcome.campaign_events
  );
  assert_eq!(
    outcome.leader_changes_after(first),
    0,
    "the derived timing held its first leader with every node alive: {:?}",
    outcome.leader_events
  );
  assert!(
    outcome.leader_at_end().is_some(),
    "one leader at the end of the run: {outcome:?}"
  );
  for report in &outcome.reports {
    assert!(
      u64::from(report.timing.base_periods) * HEARTBEAT_NS
        >= 10 * report.timing.broadcast_rtt_tail_ns,
      "{report:?}: the timeout is at least ten times the measured tail"
    );
  }
}
