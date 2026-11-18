//! A fault relay between real fleet daemons (goal g229, chaos across real processes; A-277).
//!
//! SIGSTOP stops a whole process and drops nothing, and netem needs privileges this project never takes, so the chaos
//! flows of `live_chaos.rs` put this relay between the daemons. Each node advertises a block of two ports the relay
//! owns and binds a block of its own (the manifest's `bind`, A-277). A datagram one node sends another arrives at a
//! *listener*, the relay's socket at the receiver's advertised port for that plane, and leaves for the receiver's bound
//! socket from a *flow*, a socket the relay keeps for that one sending socket. The receiver answers the flow, and the
//! answer leaves the listener for the sender. That is one outer address per inner socket, as a NAT keeps one mapping
//! per inner address, so each of a node's sessions keeps a source of its own at the receiver's demultiplexer.
//!
//! On its way, each datagram meets the rule of its path, from its sender's node to its receiver's node ([`PathRule`]):
//! a cut drops it; otherwise it is lost at the rule's rate, held for the rule's delay give or take a uniform jitter (so
//! datagrams reorder once the jitter passes their spacing), and sent twice at the duplication rate. One seed drives
//! every draw and is printed in every report, so a run's fault schedule can be named.
//!
//! A datagram's sender is the node of the socket it came from. A node's serve sockets are its bind block, claimed up
//! front ([`Relay::claim`]). Its record links dial from fresh ephemeral sockets (`fleet::client_for`), so the relay asks
//! the operating system which process holds such a port, within the process trees the test registered for each node
//! ([`Relay::root`]: a bare daemon, or an anchor and the daemons it starts): `/proc/net/udp` and `/proc/<pid>/fd` on
//! Linux, `netstat -anv -p udp` and `ps` on macOS. It counts again for every source it has not seen and once a
//! [`CENSUS_PERIOD`], and a flow whose port another process holds now is retired, as a NAT drops a mapping whose inner
//! socket is gone, so that socket's next datagram opens a flow of its own. A datagram no census attributes is dropped
//! and counted, never forwarded: under a cut, forwarding it could carry a partitioned node's traffic across.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec};

/// Format: the parts a rate is counted in ([`PathRule::loss_ppm`], [`PathRule::duplicate_ppm`]).
pub(crate) const PARTS_PER_MILLION: u32 = 1_000_000;
/// Format: the largest UDP payload; a datagram is received whole into a buffer this large.
const DATAGRAM_CAP: usize = 65_535;
/// Shape: how often the relay counts again which process holds each port, besides counting for every source it has not
/// seen: so a port that a closed socket gave up and another node's new socket took is re-attributed within a second,
/// while `netstat` on a loaded Mac (10 to 50 ms a call, 2026-11-17) stays a small share of the relay's thread.
const CENSUS_PERIOD: Duration = Duration::from_secs(1);
/// Shape: the least time between two counts asked for by unseen sources, so a source no count attributes (a process
/// outside the fleet) does not keep the relay counting.
const CENSUS_SPACING: Duration = Duration::from_millis(50);
/// Shape: the most flows the relay opens: far above a flow's need, since a five-node fleet opens forty at formation
/// (each node's probe socket and one record link to each peer) and one more a redial.
const FLOW_CAP: usize = 4096;
/// Shape: the deepest process ancestry the census walks from a socket's holder to a registered root (an anchor's
/// daemon is one below it).
const ANCESTRY_CAP: usize = 64;
/// Shape: how long the test waits for the relay to answer a report (a relay whose thread ended answers nothing).
const REPORT_WAIT: Duration = Duration::from_secs(10);

/// What the path from one node to another does to each datagram on it. The default forwards every datagram at once.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PathRule {
  /// Every datagram dropped: the path is down in this direction.
  pub(crate) cut: bool,
  /// The share of datagrams dropped, in parts per million.
  pub(crate) loss_ppm: u32,
  /// The share of datagrams sent twice, in parts per million; each copy is held on its own draw.
  pub(crate) duplicate_ppm: u32,
  /// How long a datagram is held before it is sent on.
  pub(crate) delay: Duration,
  /// The spread of the hold: uniform within `delay ± jitter`, never below zero.
  pub(crate) jitter: Duration,
}

impl PathRule {
  /// A path that is down in this direction.
  pub(crate) const CUT: PathRule = PathRule {
    cut: true,
    loss_ppm: 0,
    duplicate_ppm: 0,
    delay: Duration::ZERO,
    jitter: Duration::ZERO,
  };
}

/// What one path did, in datagrams.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PathCount {
  /// Sent on (a duplicated datagram counts once here).
  pub(crate) forwarded: u64,
  /// Dropped at the rule's loss rate.
  pub(crate) lost: u64,
  /// Dropped by a cut.
  pub(crate) cut: u64,
  /// Sent twice.
  pub(crate) duplicated: u64,
}

/// The relay's counts as of the moment it was asked.
#[derive(Clone, Debug, Default)]
pub(crate) struct RelayReport {
  /// The seed every draw came from.
  pub(crate) seed: u64,
  /// Per path, from one node's index to another's.
  pub(crate) paths: BTreeMap<(usize, usize), PathCount>,
  /// Datagrams dropped because no census named their sender.
  pub(crate) unattributed: u64,
  /// Datagrams dropped for the flow cap, a socket that refused them, a stray at a flow, or a retired flow.
  pub(crate) refused: u64,
  /// Flows opened.
  pub(crate) flows_opened: u64,
  /// Flows retired because another process holds their port now.
  pub(crate) flows_retired: u64,
  /// Censuses taken.
  pub(crate) censuses: u64,
}

impl RelayReport {
  /// One line for a verdict's notes: the seed, the attribution counts, and each path that did anything.
  pub(crate) fn summary(&self) -> String {
    let paths: Vec<String> = self
      .paths
      .iter()
      .map(|((from, to), count)| {
        format!(
          "{from}->{to} fwd {} lost {} cut {} dup {}",
          count.forwarded, count.lost, count.cut, count.duplicated
        )
      })
      .collect();
    format!(
      "relay seed {:#x}: {} flows ({} retired), {} censuses, {} unattributed, {} refused; {}",
      self.seed,
      self.flows_opened,
      self.flows_retired,
      self.censuses,
      self.unattributed,
      self.refused,
      paths.join("; ")
    )
  }

  /// The datagrams path `from -> to` dropped by its cut.
  pub(crate) fn cut(&self, from: usize, to: usize) -> u64 {
    self.paths.get(&(from, to)).map_or(0, |count| count.cut)
  }

  /// The datagrams path `from -> to` sent on.
  pub(crate) fn forwarded(&self, from: usize, to: usize) -> u64 {
    self
      .paths
      .get(&(from, to))
      .map_or(0, |count| count.forwarded)
  }
}

/// A node as the relay sees it: the sockets at its advertised block, and its bind block.
pub(crate) struct RelayedNode {
  /// The relay's sockets at the node's advertised block, bound by the test: the probe port, then the record port.
  pub(crate) advertised: [UdpSocket; 2],
  /// The node's bind block's base: what reaches its advertised probe port is sent here, and what reaches its record
  /// port to the next.
  pub(crate) bound: SocketAddrV4,
}

enum Command {
  Rule {
    from: usize,
    to: usize,
    rule: PathRule,
  },
  Root {
    node: usize,
    pid: u32,
  },
  Claim {
    port: u16,
    node: usize,
  },
  Report(mpsc::Sender<RelayReport>),
  Stop,
}

/// The test's handle on a running relay: it sets rules, names each node's processes, and reads the counts. Dropping it
/// stops the relay and joins its thread.
pub(crate) struct Relay {
  commands: mpsc::Sender<Command>,
  /// A socket of the test's own, to wake the relay's poll once a command is queued.
  waker: UdpSocket,
  wake_at: SocketAddr,
  nodes: usize,
  thread: Option<std::thread::JoinHandle<()>>,
}

impl Relay {
  /// Starts relaying for `nodes`, in node-index order, every draw from `seed`.
  pub(crate) fn start(nodes: Vec<RelayedNode>, seed: u64) -> Relay {
    let count = nodes.len();
    let wake = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    wake.set_nonblocking(true).unwrap();
    let wake_at = wake.local_addr().unwrap();
    let waker = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let (commands, received) = mpsc::channel();
    let mut listeners = Vec::with_capacity(count.saturating_mul(2));
    for (node, relayed) in nodes.into_iter().enumerate() {
      for (offset, socket) in (0u16..).zip(relayed.advertised) {
        socket.set_nonblocking(true).unwrap();
        let port = relayed.bound.port().checked_add(offset).unwrap();
        listeners.push(Listener {
          socket,
          node,
          forward_to: SocketAddrV4::new(*relayed.bound.ip(), port),
        });
      }
    }
    let state = RelayState {
      listeners,
      flows: Vec::new(),
      flow_of: BTreeMap::new(),
      nodes: count,
      rules: vec![PathRule::default(); count.saturating_mul(count)],
      roots: vec![None; count],
      claims: BTreeMap::new(),
      census: BTreeMap::new(),
      counted_at: None,
      queue: BinaryHeap::new(),
      pending: BTreeMap::new(),
      sequence: 0,
      draws: Draws(seed),
      wake,
      commands: received,
      report: RelayReport {
        seed,
        ..RelayReport::default()
      },
    };
    let thread = std::thread::Builder::new()
      .name("chaos-relay".to_owned())
      .spawn(move || state.run())
      .unwrap();
    Relay {
      commands,
      waker,
      wake_at,
      nodes: count,
      thread: Some(thread),
    }
  }

  fn send(&self, command: Command) {
    // A relay whose thread ended forwards nothing more, and the flow fails on that; the send is not the failure.
    let _ = self.commands.send(command);
    let _ = self.waker.send_to(&[0], self.wake_at);
  }

  /// The relay's counts, once every command sent before has been applied (so a rule set before it is in force).
  pub(crate) fn report(&self) -> RelayReport {
    let (answer, answered) = mpsc::channel();
    self.send(Command::Report(answer));
    answered.recv_timeout(REPORT_WAIT).unwrap_or_default()
  }

  /// Sets the rule of path `from -> to`, in force once this returns.
  pub(crate) fn rule(&self, from: usize, to: usize, rule: PathRule) {
    self.send(Command::Rule { from, to, rule });
    let _ = self.report();
  }

  /// Names `pid` as the root of node `node`'s processes: a socket held by it or by any process below it is the node's.
  pub(crate) fn root(&self, node: usize, pid: u32) {
    self.send(Command::Root { node, pid });
    let _ = self.report();
  }

  /// Names the socket at local `port` as node `node`'s, whatever a census says (a node's serve sockets, known up front).
  pub(crate) fn claim(&self, port: u16, node: usize) {
    self.send(Command::Claim { port, node });
    let _ = self.report();
  }

  /// Sets `rule` on every path into and out of `node`.
  pub(crate) fn shape_node(&self, node: usize, rule: PathRule) {
    for other in (0..self.nodes).filter(|other| *other != node) {
      self.send(Command::Rule {
        from: node,
        to: other,
        rule,
      });
      self.send(Command::Rule {
        from: other,
        to: node,
        rule,
      });
    }
    let _ = self.report();
  }

  /// Sets `rule` on every path of the fleet.
  pub(crate) fn shape_all(&self, rule: PathRule) {
    for from in 0..self.nodes {
      for to in (0..self.nodes).filter(|to| *to != from) {
        self.send(Command::Rule { from, to, rule });
      }
    }
    let _ = self.report();
  }

  /// Cuts `node` off: nothing it sends reaches a peer and nothing a peer sends reaches it, while its process runs on.
  pub(crate) fn isolate(&self, node: usize) {
    self.shape_node(node, PathRule::CUT);
  }

  /// Cuts what reaches `node` only: it still speaks to every peer and hears none.
  pub(crate) fn deafen(&self, node: usize) {
    for other in (0..self.nodes).filter(|other| *other != node) {
      self.send(Command::Rule {
        from: other,
        to: node,
        rule: PathRule::CUT,
      });
    }
    let _ = self.report();
  }

  /// Cuts what leaves `node` only: it still hears every peer and none hears it.
  pub(crate) fn mute(&self, node: usize) {
    for other in (0..self.nodes).filter(|other| *other != node) {
      self.send(Command::Rule {
        from: node,
        to: other,
        rule: PathRule::CUT,
      });
    }
    let _ = self.report();
  }

  /// Every path back to the default: every datagram forwarded at once.
  pub(crate) fn heal(&self) {
    self.shape_all(PathRule::default());
  }
}

impl Drop for Relay {
  fn drop(&mut self) {
    self.send(Command::Stop);
    if let Some(thread) = self.thread.take() {
      let _ = thread.join();
    }
  }
}

/// The relay's socket at one node's advertised port for one plane.
struct Listener {
  socket: UdpSocket,
  node: usize,
  /// The node's bound socket for the same plane.
  forward_to: SocketAddrV4,
}

/// The relay's socket for one sending socket at one listener: it sends that socket's datagrams on to the receiver and
/// receives the receiver's answers.
struct Flow {
  socket: UdpSocket,
  listener: usize,
  /// The sending socket's address.
  source: SocketAddrV4,
  /// The sending socket's node.
  sender: usize,
  /// The process the census found holding the sending socket; `None` for a claimed port, which is never retired.
  pid: Option<u32>,
}

/// Which relay socket a datagram leaves from.
#[derive(Clone, Copy)]
enum Via {
  Listener(usize),
  Flow(usize),
}

/// Which relay socket a datagram arrived at.
#[derive(Clone, Copy)]
enum Arrival {
  Wake,
  Listener(usize),
  Flow(usize),
}

/// A datagram held until its time.
struct Delivery {
  via: Via,
  to: SocketAddrV4,
  bytes: Vec<u8>,
}

/// The relay's state, owned by its thread.
struct RelayState {
  listeners: Vec<Listener>,
  /// Every flow opened, `None` once retired: a held datagram names its flow by index.
  flows: Vec<Option<Flow>>,
  flow_of: BTreeMap<(usize, SocketAddrV4), usize>,
  nodes: usize,
  /// Indexed `from * nodes + to`.
  rules: Vec<PathRule>,
  roots: Vec<Option<u32>>,
  claims: BTreeMap<u16, usize>,
  /// The last census: each UDP port held by a registered tree's process, with that process and its node.
  census: BTreeMap<u16, (u32, usize)>,
  counted_at: Option<Instant>,
  queue: BinaryHeap<Reverse<(Instant, u64)>>,
  pending: BTreeMap<u64, Delivery>,
  sequence: u64,
  draws: Draws,
  wake: UdpSocket,
  commands: mpsc::Receiver<Command>,
  report: RelayReport,
}

impl RelayState {
  fn run(mut self) {
    let mut buffer = vec![0u8; DATAGRAM_CAP];
    loop {
      loop {
        match self.commands.try_recv() {
          Ok(Command::Stop) | Err(mpsc::TryRecvError::Disconnected) => return,
          Ok(command) => self.apply(command),
          Err(mpsc::TryRecvError::Empty) => break,
        }
      }
      if self
        .counted_at
        .is_none_or(|at| at.elapsed() >= CENSUS_PERIOD)
      {
        self.count();
      }
      let now = Instant::now();
      self.deliver_due(now);
      let wait = self
        .queue
        .peek()
        .map_or(CENSUS_PERIOD, |Reverse((due, _))| {
          due.saturating_duration_since(now)
        })
        .min(CENSUS_PERIOD);
      self.poll_once(wait, &mut buffer);
    }
  }

  fn apply(&mut self, command: Command) {
    match command {
      Command::Rule { from, to, rule } => {
        if let Some(slot) = from
          .checked_mul(self.nodes)
          .and_then(|row| row.checked_add(to))
          .and_then(|index| self.rules.get_mut(index))
        {
          *slot = rule;
        }
      }
      Command::Root { node, pid } => {
        if let Some(slot) = self.roots.get_mut(node) {
          *slot = Some(pid);
        }
        self.count();
      }
      Command::Claim { port, node } => {
        self.claims.insert(port, node);
      }
      Command::Report(answer) => {
        let _ = answer.send(self.report.clone());
      }
      Command::Stop => {}
    }
  }

  /// Waits up to `wait` for any socket to be readable, then drains each readable one.
  fn poll_once(&mut self, wait: Duration, buffer: &mut [u8]) {
    let readable: Vec<Arrival> = {
      let mut fds = Vec::with_capacity(1 + self.listeners.len() + self.flows.len());
      let mut arrivals = Vec::with_capacity(fds.capacity());
      fds.push(PollFd::new(&self.wake, PollFlags::IN));
      arrivals.push(Arrival::Wake);
      for (index, listener) in self.listeners.iter().enumerate() {
        fds.push(PollFd::new(&listener.socket, PollFlags::IN));
        arrivals.push(Arrival::Listener(index));
      }
      for (index, flow) in self.flows.iter().enumerate() {
        if let Some(flow) = flow {
          fds.push(PollFd::new(&flow.socket, PollFlags::IN));
          arrivals.push(Arrival::Flow(index));
        }
      }
      let timeout = Timespec {
        tv_sec: i64::try_from(wait.as_secs()).unwrap_or(i64::MAX),
        tv_nsec: wait.subsec_nanos().into(),
      };
      // An interrupted poll returns early; the loop polls again.
      if rustix::event::poll(&mut fds, Some(&timeout)).is_err() {
        return;
      }
      fds
        .iter()
        .zip(arrivals)
        .filter(|(fd, _)| !fd.revents().is_empty())
        .map(|(_, arrival)| arrival)
        .collect()
    };
    for arrival in readable {
      self.drain(arrival, buffer);
    }
  }

  /// Receives every datagram waiting at one socket.
  fn drain(&mut self, arrival: Arrival, buffer: &mut [u8]) {
    loop {
      let received = match arrival {
        Arrival::Wake => self.wake.recv_from(buffer),
        Arrival::Listener(index) => match self.listeners.get(index) {
          Some(listener) => listener.socket.recv_from(buffer),
          None => return,
        },
        Arrival::Flow(index) => match self.flows.get(index).and_then(Option::as_ref) {
          Some(flow) => flow.socket.recv_from(buffer),
          None => return,
        },
      };
      // Nothing more waiting (or an error the socket reported once, such as a port-unreachable an earlier send drew):
      // the next poll sees whatever is left.
      let Ok((length, SocketAddr::V4(from))) = received else {
        return;
      };
      let bytes = buffer.get(..length).unwrap_or_default().to_vec();
      match arrival {
        Arrival::Wake => {}
        Arrival::Listener(index) => self.relay_from_sender(index, from, bytes),
        Arrival::Flow(index) => self.relay_from_receiver(index, from, bytes),
      }
    }
  }

  /// A datagram a node's socket sent to listener `listener`: on, through the flow of its socket.
  fn relay_from_sender(&mut self, listener: usize, source: SocketAddrV4, bytes: Vec<u8>) {
    let Some(flow) = self.flow_for(listener, source) else {
      return;
    };
    let (Some((receiver, to)), Some(sender)) = (
      self
        .listeners
        .get(listener)
        .map(|listener| (listener.node, listener.forward_to)),
      self
        .flows
        .get(flow)
        .and_then(Option::as_ref)
        .map(|flow| flow.sender),
    ) else {
      return;
    };
    self.route(sender, receiver, Via::Flow(flow), to, bytes);
  }

  /// A datagram the receiver's socket answered flow `flow` with: back, out of the flow's listener, to its sender.
  fn relay_from_receiver(&mut self, flow: usize, from: SocketAddrV4, bytes: Vec<u8>) {
    let Some(found) = self.flows.get(flow).and_then(Option::as_ref) else {
      return;
    };
    let Some(listener) = self.listeners.get(found.listener) else {
      return;
    };
    if from != listener.forward_to {
      // Only the receiver's bound socket answers a flow; anything else is a stray.
      self.report.refused += 1;
      return;
    }
    let (sender, receiver, to, via) = (
      listener.node,
      found.sender,
      found.source,
      Via::Listener(found.listener),
    );
    self.route(sender, receiver, via, to, bytes);
  }

  /// The flow for `source` at `listener`, opened on its first datagram once a census names its node.
  fn flow_for(&mut self, listener: usize, source: SocketAddrV4) -> Option<usize> {
    if let Some(index) = self.flow_of.get(&(listener, source)) {
      return Some(*index);
    }
    let Some((sender, pid)) = self.attribute(source.port()) else {
      self.report.unattributed += 1;
      return None;
    };
    if self.flows.len() >= FLOW_CAP {
      self.report.refused += 1;
      return None;
    }
    let Some(socket) = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
      .ok()
      .filter(|socket| socket.set_nonblocking(true).is_ok())
    else {
      self.report.refused += 1;
      return None;
    };
    let index = self.flows.len();
    self.flows.push(Some(Flow {
      socket,
      listener,
      source,
      sender,
      pid,
    }));
    self.flow_of.insert((listener, source), index);
    self.report.flows_opened += 1;
    Some(index)
  }

  /// The node of the socket at local `port`, and the process holding it when a census found it: a claim first, then the
  /// last census, then a new census if none was taken within [`CENSUS_SPACING`].
  fn attribute(&mut self, port: u16) -> Option<(usize, Option<u32>)> {
    if let Some(node) = self.claims.get(&port) {
      return Some((*node, None));
    }
    if !self.census.contains_key(&port)
      && self
        .counted_at
        .is_none_or(|at| at.elapsed() >= CENSUS_SPACING)
    {
      self.count();
    }
    self
      .census
      .get(&port)
      .map(|(pid, node)| (*node, Some(*pid)))
  }

  /// Takes a census, and retires every flow whose port another process holds now: that is a new socket, and its next
  /// datagram opens a flow of its own.
  fn count(&mut self) {
    self.census = census(&self.roots);
    self.counted_at = Some(Instant::now());
    self.report.censuses += 1;
    let retired: Vec<usize> = self
      .flows
      .iter()
      .enumerate()
      .filter_map(|(index, flow)| {
        let flow = flow.as_ref()?;
        let pid = flow.pid?;
        match self.census.get(&flow.source.port()) {
          Some((holder, _)) if *holder != pid => Some(index),
          _ => None,
        }
      })
      .collect();
    for index in retired {
      if let Some(flow) = self.flows.get_mut(index).and_then(Option::take) {
        self.flow_of.remove(&(flow.listener, flow.source));
        self.report.flows_retired += 1;
      }
    }
  }

  /// Applies path `from -> to`'s rule to one datagram bound for `to_address` through `via`.
  fn route(&mut self, from: usize, to: usize, via: Via, to_address: SocketAddrV4, bytes: Vec<u8>) {
    let rule = from
      .checked_mul(self.nodes)
      .and_then(|row| row.checked_add(to))
      .and_then(|index| self.rules.get(index))
      .copied()
      .unwrap_or_default();
    let lost = !rule.cut && self.draws.chance(rule.loss_ppm);
    let duplicated = !rule.cut && !lost && self.draws.chance(rule.duplicate_ppm);
    let count = self.report.paths.entry((from, to)).or_default();
    if rule.cut {
      count.cut += 1;
      return;
    }
    if lost {
      count.lost += 1;
      return;
    }
    count.forwarded += 1;
    if duplicated {
      count.duplicated += 1;
      let hold = self.draws.hold(rule.delay, rule.jitter);
      self.schedule(
        hold,
        Delivery {
          via,
          to: to_address,
          bytes: bytes.clone(),
        },
      );
    }
    let hold = self.draws.hold(rule.delay, rule.jitter);
    self.schedule(
      hold,
      Delivery {
        via,
        to: to_address,
        bytes,
      },
    );
  }

  /// Sends `delivery` after `hold`: at once when there is none.
  fn schedule(&mut self, hold: Duration, delivery: Delivery) {
    if hold.is_zero() {
      self.send(delivery);
      return;
    }
    let sequence = self.sequence;
    self.sequence = self.sequence.wrapping_add(1);
    self.queue.push(Reverse((Instant::now() + hold, sequence)));
    self.pending.insert(sequence, delivery);
  }

  /// Sends every held datagram whose time has come, in time order.
  fn deliver_due(&mut self, now: Instant) {
    while let Some(Reverse((due, sequence))) = self.queue.peek().copied() {
      if due > now {
        break;
      }
      self.queue.pop();
      if let Some(delivery) = self.pending.remove(&sequence) {
        self.send(delivery);
      }
    }
  }

  fn send(&mut self, delivery: Delivery) {
    let socket = match delivery.via {
      Via::Listener(index) => self.listeners.get(index).map(|listener| &listener.socket),
      Via::Flow(index) => self
        .flows
        .get(index)
        .and_then(Option::as_ref)
        .map(|flow| &flow.socket),
    };
    let sent = socket.is_some_and(|socket| socket.send_to(&delivery.bytes, delivery.to).is_ok());
    if !sent {
      self.report.refused += 1;
    }
  }
}

/// SplitMix64 (Steele, Lea and Flood, "Fast splittable pseudorandom number generators", OOPSLA 2014; the constants and
/// shifts as Vigna's reference publishes them): a reproducible sequence of draws from one seed. The relay draws its
/// rules' outcomes from one; a flow's own schedule (the nemesis of `live_chaos.rs`) draws from another.
pub(crate) struct Draws(u64);

impl Draws {
  /// Format: SplitMix64's increment, the 64-bit fraction of the golden ratio.
  const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;
  /// Format: SplitMix64's first mixing multiplier.
  const MIX_FIRST: u64 = 0xBF58_476D_1CE4_E5B9;
  /// Format: SplitMix64's second mixing multiplier.
  const MIX_SECOND: u64 = 0x94D0_49BB_1331_11EB;

  /// The sequence `seed` names.
  pub(crate) fn seeded(seed: u64) -> Draws {
    Draws(seed)
  }

  /// A draw in `0..bound` (zero for a bound of zero). The modulo's bias is below `bound / 2^64`, nothing at the bounds
  /// a schedule draws.
  pub(crate) fn below(&mut self, bound: u64) -> u64 {
    self.next() % bound.max(1)
  }

  /// A span uniform within `low..=high` (`low` when `high` is not above it).
  pub(crate) fn within(&mut self, low: Duration, high: Duration) -> Duration {
    let span_ns = u64::try_from(high.saturating_sub(low).as_nanos()).unwrap_or(u64::MAX);
    low.saturating_add(Duration::from_nanos(self.below(span_ns.saturating_add(1))))
  }

  fn next(&mut self) -> u64 {
    self.0 = self.0.wrapping_add(Self::GAMMA);
    let mut mixed = self.0;
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(Self::MIX_FIRST);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(Self::MIX_SECOND);
    mixed ^ (mixed >> 31)
  }

  /// True with probability `ppm` parts per million.
  fn chance(&mut self, ppm: u32) -> bool {
    ppm > 0 && self.next() % u64::from(PARTS_PER_MILLION) < u64::from(ppm)
  }

  /// A hold uniform within `delay ± jitter`, never below zero.
  fn hold(&mut self, delay: Duration, jitter: Duration) -> Duration {
    let jitter_ns = u64::try_from(jitter.as_nanos()).unwrap_or(u64::MAX);
    if jitter_ns == 0 {
      return delay;
    }
    let offset = self.next() % jitter_ns.saturating_mul(2).saturating_add(1);
    let delay_ns = u64::try_from(delay.as_nanos()).unwrap_or(u64::MAX);
    Duration::from_nanos(delay_ns.saturating_add(offset).saturating_sub(jitter_ns))
  }
}

/// Each process in the trees `roots` names, a root and everything below it, with its node: a process belongs to the
/// first root met walking up its ancestry (at most [`ANCESTRY_CAP`] steps), given `parents` (each pid's parent).
fn trees(roots: &[Option<u32>], parents: &BTreeMap<u32, u32>) -> BTreeMap<u32, usize> {
  let mut members: BTreeMap<u32, usize> = roots
    .iter()
    .enumerate()
    .filter_map(|(node, root)| root.map(|pid| (pid, node)))
    .collect();
  for &pid in parents.keys() {
    let mut chain = Vec::new();
    let mut at = pid;
    let found = loop {
      if let Some(node) = members.get(&at) {
        break Some(*node);
      }
      chain.push(at);
      match parents.get(&at) {
        Some(&parent) if parent != at && parent != 0 && chain.len() <= ANCESTRY_CAP => at = parent,
        _ => break None,
      }
    };
    if let Some(node) = found {
      for member in chain {
        members.insert(member, node);
      }
    }
  }
  members
}

/// Every UDP port held by a process of the trees `roots` names: the port, the holding process and its node. Linux reads
/// `/proc`: each tree process's descriptors name socket inodes (`/proc/<pid>/fd`), and `/proc/net/udp` names each
/// inode's local port.
#[cfg(target_os = "linux")]
fn census(roots: &[Option<u32>]) -> BTreeMap<u16, (u32, usize)> {
  let mut parents = BTreeMap::new();
  if let Ok(entries) = std::fs::read_dir("/proc") {
    for entry in entries.flatten() {
      let Some(pid) = entry
        .file_name()
        .to_str()
        .and_then(|name| name.parse::<u32>().ok())
      else {
        continue;
      };
      // `pid (comm) state ppid ...`: the command may hold spaces and parentheses, so read after its last `)`.
      let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        continue;
      };
      if let Some(parent) = stat
        .rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().nth(1))
        .and_then(|parent| parent.parse::<u32>().ok())
      {
        parents.insert(pid, parent);
      }
    }
  }
  let members = trees(roots, &parents);
  let mut inodes: BTreeMap<u64, (u32, usize)> = BTreeMap::new();
  for (&pid, &node) in &members {
    let Ok(descriptors) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
      continue;
    };
    for descriptor in descriptors.flatten() {
      let Ok(target) = std::fs::read_link(descriptor.path()) else {
        continue;
      };
      if let Some(inode) = target
        .to_str()
        .and_then(|target| target.strip_prefix("socket:["))
        .and_then(|target| target.strip_suffix(']'))
        .and_then(|inode| inode.parse::<u64>().ok())
      {
        inodes.insert(inode, (pid, node));
      }
    }
  }
  let mut ports = BTreeMap::new();
  for table in ["/proc/net/udp", "/proc/net/udp6"] {
    let Ok(text) = std::fs::read_to_string(table) else {
      continue;
    };
    // `sl local_address rem_address st tx:rx tr:when retrnsmt uid timeout inode ...`, the port in hex.
    for line in text.lines().skip(1) {
      let fields: Vec<&str> = line.split_whitespace().collect();
      let port = fields
        .get(1)
        .and_then(|local| local.rsplit_once(':'))
        .and_then(|(_, port)| u16::from_str_radix(port, 16).ok());
      let inode = fields.get(9).and_then(|inode| inode.parse::<u64>().ok());
      if let (Some(port), Some(inode)) = (port, inode)
        && let Some(owner) = inodes.get(&inode)
      {
        ports.insert(port, *owner);
      }
    }
  }
  ports
}

/// Every UDP port held by a process of the trees `roots` names: the port, the holding process and its node. macOS asks
/// `ps` for each process's parent and `netstat -anv -p udp` for each socket's port and holder (its `name:pid` column;
/// a name may hold spaces, so the holder is the field that ends in a tree process's pid).
#[cfg(target_os = "macos")]
fn census(roots: &[Option<u32>]) -> BTreeMap<u16, (u32, usize)> {
  let output = |program: &str, args: &[&str]| {
    std::process::Command::new(program)
      .args(args)
      .output()
      .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
      .unwrap_or_default()
  };
  let parents: BTreeMap<u32, u32> = output("ps", &["-A", "-o", "pid=,ppid="])
    .lines()
    .filter_map(|line| {
      let mut fields = line.split_whitespace();
      Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
    })
    .collect();
  let members = trees(roots, &parents);
  let mut ports = BTreeMap::new();
  for line in output("netstat", &["-anv", "-p", "udp"])
    .lines()
    .filter(|line| line.starts_with("udp4"))
  {
    // `Proto Recv-Q Send-Q Local-Address ...`: the local address is `ip.port` (or `*.port`).
    let port = line
      .split_whitespace()
      .nth(3)
      .and_then(|local| local.rsplit_once('.'))
      .and_then(|(_, port)| port.parse::<u16>().ok());
    let owner = line.split_whitespace().find_map(|field| {
      let pid = field.rsplit_once(':')?.1.parse::<u32>().ok()?;
      members.get(&pid).map(|node| (pid, *node))
    });
    if let (Some(port), Some(owner)) = (port, owner) {
      ports.insert(port, owner);
    }
  }
  ports
}

/// No census on another platform: every source must be claimed.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn census(_roots: &[Option<u32>]) -> BTreeMap<u16, (u32, usize)> {
  BTreeMap::new()
}

/// Whether this platform can attribute a socket by its process ([`census`]).
pub(crate) fn census_attributes() -> bool {
  cfg!(any(target_os = "linux", target_os = "macos"))
}

// ── The relay's own tests: in-process sockets, no daemon, so they run without `SLATES_TEST_CLI` ─────────────────────

/// Shape: how long a test waits for a datagram the relay should forward.
#[cfg(test)]
const ARRIVAL_WAIT: Duration = Duration::from_secs(5);
/// Shape: how long a test waits to be sure a datagram the relay should drop did not arrive.
#[cfg(test)]
const ABSENCE_WAIT: Duration = Duration::from_millis(300);
/// Shape: the hold a test sets, long enough to tell from the loopback's own microseconds.
#[cfg(test)]
const TEST_DELAY: Duration = Duration::from_millis(60);
/// Shape: how many bases a test tries for a pair of consecutive free ports.
#[cfg(test)]
const PAIR_ATTEMPTS: usize = 200;

/// Two sockets at consecutive loopback ports, as a node's port block is laid out.
#[cfg(test)]
fn consecutive_pair() -> [UdpSocket; 2] {
  for _ in 0..PAIR_ATTEMPTS {
    let first = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = first.local_addr().unwrap().port();
    if let Some(next) = port.checked_add(1)
      && let Ok(second) = UdpSocket::bind((Ipv4Addr::LOCALHOST, next))
    {
      return [first, second];
    }
  }
  panic!("no two consecutive free loopback ports in {PAIR_ATTEMPTS} tries");
}

#[cfg(test)]
fn local(socket: &UdpSocket) -> SocketAddrV4 {
  match socket.local_addr().unwrap() {
    SocketAddr::V4(address) => address,
    SocketAddr::V6(address) => panic!("a loopback IPv4 socket, not {address}"),
  }
}

/// One test node: its bound probe and record sockets, its advertised addresses, and a socket it dials from.
#[cfg(test)]
struct TestNode {
  bound: [UdpSocket; 2],
  advertised_probe: SocketAddrV4,
  advertised_record: SocketAddrV4,
  dialer: UdpSocket,
}

/// Two test nodes behind a relay seeded `seed`, every socket claimed for its node (one process holds them all).
#[cfg(test)]
fn relayed_pair(seed: u64) -> (Relay, Vec<TestNode>) {
  let mut relayed = Vec::new();
  let mut nodes = Vec::new();
  for _ in 0..2 {
    let bound = consecutive_pair();
    let advertised = consecutive_pair();
    let dialer = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    for socket in bound.iter().chain([&dialer]) {
      socket.set_read_timeout(Some(ARRIVAL_WAIT)).unwrap();
    }
    nodes.push(TestNode {
      advertised_probe: local(&advertised[0]),
      advertised_record: local(&advertised[1]),
      bound,
      dialer,
    });
    relayed.push(RelayedNode {
      bound: local(&nodes.last().unwrap().bound[0]),
      advertised,
    });
  }
  let relay = Relay::start(relayed, seed);
  for (index, node) in nodes.iter().enumerate() {
    for socket in node.bound.iter().chain([&node.dialer]) {
      relay.claim(local(socket).port(), index);
    }
  }
  (relay, nodes)
}

/// Receives one datagram at `socket` within `wait`: its bytes and source, or `None`.
#[cfg(test)]
fn receive_within(socket: &UdpSocket, wait: Duration) -> Option<(Vec<u8>, SocketAddrV4)> {
  socket.set_read_timeout(Some(wait)).unwrap();
  let mut buffer = [0u8; 2048];
  match socket.recv_from(&mut buffer) {
    Ok((length, SocketAddr::V4(from))) => Some((buffer[..length].to_vec(), from)),
    _ => None,
  }
}

/// Goal g229 (the relay under every chaos flow). Do: node 0 dials node 1's advertised record port through the relay,
/// and node 1 answers the source it saw. Expect: the datagram reaches node 1's bound record socket from a relay flow,
/// not from node 0's own socket, and the answer reaches node 0's dialer from node 1's advertised record port, as a NAT
/// would deliver it; and a probe to the advertised probe port reaches the bound probe socket.
#[test]
fn a_relayed_datagram_reaches_the_bound_socket_and_its_answer_comes_back() {
  let (relay, nodes) = relayed_pair(1);
  nodes[0]
    .dialer
    .send_to(b"ping", nodes[1].advertised_record)
    .unwrap();
  let (bytes, flow) = receive_within(&nodes[1].bound[1], ARRIVAL_WAIT)
    .expect("node 1's record socket got the datagram");
  assert_eq!(bytes, b"ping");
  assert_ne!(
    flow,
    local(&nodes[0].dialer),
    "the relay forwards from a flow of its own"
  );
  nodes[1].bound[1].send_to(b"pong", flow).unwrap();
  let (bytes, from) =
    receive_within(&nodes[0].dialer, ARRIVAL_WAIT).expect("node 0's dialer got the answer");
  assert_eq!(bytes, b"pong");
  assert_eq!(
    from, nodes[1].advertised_record,
    "the answer comes from the advertised port"
  );
  nodes[0].bound[0]
    .send_to(b"probe", nodes[1].advertised_probe)
    .unwrap();
  let (bytes, _) =
    receive_within(&nodes[1].bound[0], ARRIVAL_WAIT).expect("node 1's probe socket got the probe");
  assert_eq!(bytes, b"probe");
  let report = relay.report();
  assert_eq!(report.forwarded(0, 1), 2, "{}", report.summary());
  assert_eq!(report.forwarded(1, 0), 1, "{}", report.summary());
}

/// Goal g229, a one-way cut. Do: cut path 0 -> 1, then send from node 0 to node 1, and from node 1 to node 0 with node
/// 0 answering. Expect: nothing of node 0's reaches node 1, its answer included (it travels 0 -> 1), while node 1's
/// datagram reaches node 0; the cut counted on 0 -> 1 only. Then heal: node 0's datagram arrives.
#[test]
fn a_cut_drops_its_direction_only_until_healed() {
  let (relay, nodes) = relayed_pair(2);
  relay.rule(0, 1, PathRule::CUT);
  nodes[0]
    .dialer
    .send_to(b"lost", nodes[1].advertised_record)
    .unwrap();
  assert_eq!(
    receive_within(&nodes[1].bound[1], ABSENCE_WAIT),
    None,
    "a cut path delivers nothing"
  );
  nodes[1]
    .dialer
    .send_to(b"heard", nodes[0].advertised_record)
    .unwrap();
  let (bytes, flow) =
    receive_within(&nodes[0].bound[1], ARRIVAL_WAIT).expect("the open direction delivers");
  assert_eq!(bytes, b"heard");
  nodes[0].bound[1].send_to(b"unheard", flow).unwrap();
  assert_eq!(
    receive_within(&nodes[1].dialer, ABSENCE_WAIT),
    None,
    "an answer on the cut direction is cut too"
  );
  let report = relay.report();
  assert_eq!(report.cut(0, 1), 2, "{}", report.summary());
  assert_eq!(report.cut(1, 0), 0, "{}", report.summary());
  relay.heal();
  nodes[0]
    .dialer
    .send_to(b"healed", nodes[1].advertised_record)
    .unwrap();
  let (bytes, _) =
    receive_within(&nodes[1].bound[1], ARRIVAL_WAIT).expect("a healed path delivers");
  assert_eq!(bytes, b"healed");
}

/// Goal g229, the one-way partitions a flow applies to a whole node. Do: mute node 0 and send each way; heal; deafen node
/// 0 and send each way. Expect: muted, node 0's datagram is cut and node 1's arrives; deafened, the reverse; each cut
/// counted on its own direction.
#[test]
fn mute_and_deafen_each_cut_one_direction_of_a_node() {
  let (relay, nodes) = relayed_pair(5);
  relay.mute(0);
  nodes[0]
    .dialer
    .send_to(b"unheard", nodes[1].advertised_record)
    .unwrap();
  nodes[1]
    .dialer
    .send_to(b"heard", nodes[0].advertised_record)
    .unwrap();
  let (bytes, _) =
    receive_within(&nodes[0].bound[1], ARRIVAL_WAIT).expect("a muted node still hears");
  assert_eq!(bytes, b"heard");
  assert_eq!(
    receive_within(&nodes[1].bound[1], ABSENCE_WAIT),
    None,
    "nobody hears a muted node"
  );
  let report = relay.report();
  assert_eq!(
    (report.cut(0, 1), report.cut(1, 0)),
    (1, 0),
    "{}",
    report.summary()
  );
  relay.heal();
  relay.deafen(0);
  nodes[0]
    .dialer
    .send_to(b"spoken", nodes[1].advertised_record)
    .unwrap();
  nodes[1]
    .dialer
    .send_to(b"lost", nodes[0].advertised_record)
    .unwrap();
  let (bytes, _) =
    receive_within(&nodes[1].bound[1], ARRIVAL_WAIT).expect("a deafened node still speaks");
  assert_eq!(bytes, b"spoken");
  assert_eq!(
    receive_within(&nodes[0].bound[1], ABSENCE_WAIT),
    None,
    "a deafened node hears nothing"
  );
  let report = relay.report();
  assert_eq!(
    (report.cut(0, 1), report.cut(1, 0)),
    (1, 1),
    "{}",
    report.summary()
  );
}

/// Goal g229, the rates and the hold. Do: send node 0 -> 1 under all-loss, then under a fixed delay, then under
/// all-duplication. Expect: nothing arrives and the loss is counted; the delayed datagram arrives no sooner than the
/// delay; the duplicated one arrives twice.
#[test]
fn loss_delay_and_duplication_follow_the_rule() {
  let (relay, nodes) = relayed_pair(3);
  relay.rule(
    0,
    1,
    PathRule {
      loss_ppm: PARTS_PER_MILLION,
      ..PathRule::default()
    },
  );
  nodes[0]
    .dialer
    .send_to(b"lost", nodes[1].advertised_record)
    .unwrap();
  assert_eq!(
    receive_within(&nodes[1].bound[1], ABSENCE_WAIT),
    None,
    "all-loss delivers nothing"
  );
  assert_eq!(
    relay.report().paths.get(&(0, 1)).map(|count| count.lost),
    Some(1)
  );

  relay.rule(
    0,
    1,
    PathRule {
      delay: TEST_DELAY,
      ..PathRule::default()
    },
  );
  let sent = Instant::now();
  nodes[0]
    .dialer
    .send_to(b"held", nodes[1].advertised_record)
    .unwrap();
  let (bytes, _) =
    receive_within(&nodes[1].bound[1], ARRIVAL_WAIT).expect("a held datagram arrives");
  assert_eq!(bytes, b"held");
  assert!(
    sent.elapsed() >= TEST_DELAY,
    "held {:?}, less than the rule's {TEST_DELAY:?}",
    sent.elapsed()
  );

  relay.rule(
    0,
    1,
    PathRule {
      duplicate_ppm: PARTS_PER_MILLION,
      ..PathRule::default()
    },
  );
  nodes[0]
    .dialer
    .send_to(b"twice", nodes[1].advertised_record)
    .unwrap();
  for copy in 1..=2 {
    let (bytes, _) = receive_within(&nodes[1].bound[1], ARRIVAL_WAIT)
      .unwrap_or_else(|| panic!("copy {copy} arrives"));
    assert_eq!(bytes, b"twice");
  }
}

/// Goal g229, attribution by process. Do: bind a socket in this process, name this process as node 0's root, and take
/// a census; then send from an unclaimed socket of this process through a relay that knows this process as node 0's.
/// Expect: the census names the socket's port under this process and node 0, and the relay forwards the unclaimed
/// socket's datagram on path 0 -> 1. Skips loudly where no census can attribute (neither Linux nor macOS).
#[test]
fn a_census_attributes_a_socket_to_its_process() {
  if !census_attributes() {
    eprintln!("skipping the census test: no process census on this platform");
    return;
  }
  let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
  let port = local(&socket).port();
  let own = std::process::id();
  let found = census(&[Some(own)]);
  assert_eq!(
    found.get(&port),
    Some(&(own, 0)),
    "the census found port {port} under this process"
  );

  let (relay, nodes) = relayed_pair(4);
  relay.root(0, own);
  socket
    .send_to(b"counted", nodes[1].advertised_record)
    .unwrap();
  let (bytes, _) = receive_within(&nodes[1].bound[1], ARRIVAL_WAIT)
    .expect("an attributed socket's datagram arrives");
  assert_eq!(bytes, b"counted");
  let report = relay.report();
  assert_eq!(report.unattributed, 0, "{}", report.summary());
  assert_eq!(report.forwarded(0, 1), 1, "{}", report.summary());
}

/// Goal g229: the draws are SplitMix64's, so a seed names a schedule. Do: draw holds and chances from two relays'
/// generators seeded alike and one seeded otherwise; then a schedule's spans and picks. Expect: equal seeds, equal
/// draws; every hold within `delay ± jitter`; a zero rate never fires and a whole one always does; every span within
/// its bounds, every pick below its bound and each of a small bound's values drawn.
#[test]
fn the_draws_are_reproducible_and_within_their_bounds() {
  let (mut first, mut second, mut other) = (Draws(7), Draws(7), Draws(8));
  let delay = Duration::from_millis(20);
  let jitter = Duration::from_millis(5);
  let firsts: Vec<Duration> = (0..1000).map(|_| first.hold(delay, jitter)).collect();
  let seconds: Vec<Duration> = (0..1000).map(|_| second.hold(delay, jitter)).collect();
  let others: Vec<Duration> = (0..1000).map(|_| other.hold(delay, jitter)).collect();
  assert_eq!(firsts, seconds);
  assert_ne!(firsts, others);
  assert!(
    firsts
      .iter()
      .all(|hold| *hold >= delay - jitter && *hold <= delay + jitter)
  );
  assert!((0..1000).all(|_| !first.chance(0)));
  assert!((0..1000).all(|_| first.chance(PARTS_PER_MILLION)));
  expect_the_schedules_draws_within_their_bounds();
}

/// The schedule's half of `the_draws_are_reproducible_and_within_their_bounds`. Do: draw spans and picks from two
/// generators seeded alike. Expect every span within its bounds, one seed one sequence, every value of a small bound
/// drawn, a bound of zero drawing zero, and a span whose upper bound is not above its lower drawing the lower.
fn expect_the_schedules_draws_within_their_bounds() {
  let (low, high) = (Duration::from_millis(200), Duration::from_secs(4));
  let (mut schedule, mut again) = (Draws::seeded(11), Draws::seeded(11));
  let spans: Vec<Duration> = (0..1000).map(|_| schedule.within(low, high)).collect();
  assert!(spans.iter().all(|span| *span >= low && *span <= high));
  assert_eq!(
    spans,
    (0..1000)
      .map(|_| again.within(low, high))
      .collect::<Vec<_>>()
  );
  let picks: std::collections::BTreeSet<u64> = (0..1000).map(|_| schedule.below(3)).collect();
  assert_eq!(picks, (0..3).collect());
  assert_eq!(schedule.below(0), 0, "a bound of zero draws zero");
  assert_eq!(
    schedule.within(high, low),
    high,
    "a span whose upper bound is not above its lower is the lower"
  );
}
