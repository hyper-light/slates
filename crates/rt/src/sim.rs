//! The simulation driver and runtime: virtual time, a seeded generator, kicks as flags, and fault
//! injection, so a whole set of shards runs deterministically on one thread (§4.3, D-20).
//!
//! Every simulated shard shares one clock. `SimRuntime::run_until_idle` steps each shard in
//! turn; when all are idle it advances the clock to the earliest deadline any shard asked for,
//! and stops when no shard has a deadline or a message. Fault injection: `kill_driver` makes a
//! shard's next wait fail with `DriverLost`, which the shard answers by cancelling every task
//! with a terminal completion and exiting (T-0.7). The registry owns each shard's flags.
//! Kicks carry a registration generation and pin the flags for their borrow; the owning
//! driver borrows them until its context ends. The runtime owns the shared clock.
//!
//! The simulated UDP fabric below carries the fleet plane at N=1 and models the network under test
//! ([`SimPath`]): a one-way delay with seeded jitter (since 2026-09-14, so the consensus timing rules of
//! §4.8 are provable on a WAN profile, `docs/wip/wan-timeout.md`), and since 2026-09-27 a bottleneck link
//! with a drop-tail queue shared by the flows through it ([`SimLink`]), Gilbert–Elliott random and burst
//! loss ([`SimLoss`]), a path MTU, a bounded receive buffer, and a NAT whose mapping expires and rebinds
//! ([`SimNat`]) — every condition the session plane's constrained-link design is proven against
//! (`docs/wip/research/nfs-transport-constrained-links.md` §5, §9), deterministic from the seed.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use slates_machine::stats::Xorshift;

use crate::driver::{Completion, Driver, DriverKind, DriverSeed, Kick};
use crate::error::RtError;
use crate::runtime::RuntimeConfig;
use crate::shard::{ShardContext, ShardId, ShardSeed, StepOutcome, TaskId};
use crate::task::SpawnRequest;

/// Format: the "no deadline requested" sentinel.
const NO_DEADLINE: u64 = u64::MAX;

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};

use slates_mem::Encoded;

/// Format: parts per million, the unit every probability of the path model is stated in (an integer
/// so a profile is exact, comparable and replayable; one million is certainty).
pub const PPM: u32 = 1_000_000;

/// Format: nanoseconds per second, for the serialization time of a datagram at a link's bit rate.
const NANOS_PER_SECOND: u128 = 1_000_000_000;

/// Format: bits per byte.
const BITS_PER_BYTE: u128 = 8;

/// A packet-loss process on a modelled path: the two-state Gilbert–Elliott channel [A: Gilbert, BSTJ
/// 1960; Elliott, BSTJ 1963], the standard model of both independent loss (one state) and bursty loss
/// (a "bad" state entered and left with the given per-datagram probabilities). Every probability is in
/// parts per million ([`PPM`]); the state is kept per directed flow and drawn from the fabric's seeded
/// generator, so a scenario replays exactly from its seed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SimLoss {
  /// Per-datagram probability of moving from the good state to the bad one.
  good_to_bad_ppm: u32,
  /// Per-datagram probability of moving from the bad state back to the good one.
  bad_to_good_ppm: u32,
  /// Loss probability of a datagram sent in the good state.
  good_loss_ppm: u32,
  /// Loss probability of a datagram sent in the bad state.
  bad_loss_ppm: u32,
}

impl SimLoss {
  /// No loss.
  pub const NONE: SimLoss = SimLoss {
    good_to_bad_ppm: 0,
    bad_to_good_ppm: PPM,
    good_loss_ppm: 0,
    bad_loss_ppm: 0,
  };

  /// Independent (Bernoulli) loss of `loss_ppm` per datagram.
  pub const fn random(loss_ppm: u32) -> SimLoss {
    SimLoss {
      good_to_bad_ppm: 0,
      bad_to_good_ppm: PPM,
      good_loss_ppm: loss_ppm,
      bad_loss_ppm: loss_ppm,
    }
  }

  /// Bursty loss: the channel enters a burst with probability `enter_ppm` per datagram, leaves it with
  /// `leave_ppm` (so a burst lasts `PPM / leave_ppm` datagrams on average), and loses `burst_loss_ppm`
  /// of the datagrams sent inside a burst and none outside it.
  pub const fn bursty(enter_ppm: u32, leave_ppm: u32, burst_loss_ppm: u32) -> SimLoss {
    SimLoss {
      good_to_bad_ppm: enter_ppm,
      bad_to_good_ppm: leave_ppm,
      good_loss_ppm: 0,
      bad_loss_ppm: burst_loss_ppm,
    }
  }

  /// Whether this process ever loses anything (so a loss-free path draws nothing from the generator).
  const fn is_lossless(&self) -> bool {
    self.good_loss_ppm == 0 && self.bad_loss_ppm == 0
  }
}

/// A draw that succeeds with probability `ppm` parts per million.
fn chance(rng: &mut Xorshift, ppm: u32) -> bool {
  let bound = usize::try_from(PPM).unwrap_or(usize::MAX);
  u32::try_from(rng.below(bound)).unwrap_or(PPM) < ppm
}

/// A bottleneck link on the simulated fabric: datagrams through it are serialized at its bit rate one
/// after another, and wait in its drop-tail queue of `queue_bytes` while it is busy — a datagram that
/// would overflow the queue is dropped (the congestion loss a sender's controller must react to, and the
/// standing queue whose delay bufferbloat is). Several directed paths may name one link, so their flows
/// compete for its capacity (the single-bottleneck "dumbbell" of congestion-control evaluation, RFC 5166).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimLink {
  /// The link's capacity in bits per second.
  pub rate_bits_per_second: u64,
  /// The queue ahead of the link, in bytes; a datagram arriving when the backlog plus itself exceeds it
  /// is dropped.
  pub queue_bytes: u64,
}

/// A link added to this thread's fabric ([`sim_udp_add_link`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SimLinkId(u32);

/// A link's running state: its configuration and the instant its transmitter is next free.
#[derive(Debug)]
struct LinkState {
  link: SimLink,
  busy_until_ns: u64,
}

impl LinkState {
  /// The time `bytes` occupy the link at its rate, nanoseconds (rounded up, so a datagram always takes
  /// time on a finite link).
  fn serialization_ns(&self, bytes: usize) -> u64 {
    let rate = u128::from(self.link.rate_bits_per_second.max(1));
    let bits = (bytes as u128).saturating_mul(BITS_PER_BYTE);
    u64::try_from((bits.saturating_mul(NANOS_PER_SECOND)).div_ceil(rate)).unwrap_or(u64::MAX)
  }

  /// The bytes waiting ahead of a datagram arriving at `now` — the backlog the transmitter still has to
  /// send, from how long it stays busy at its rate.
  fn backlog_bytes(&self, now: u64) -> u64 {
    let busy_ns = u128::from(self.busy_until_ns.saturating_sub(now));
    let bits =
      busy_ns.saturating_mul(u128::from(self.link.rate_bits_per_second)) / NANOS_PER_SECOND;
    u64::try_from(bits / BITS_PER_BYTE).unwrap_or(u64::MAX)
  }
}

/// The model of one directed path on the simulated fabric (§4.8 A-9; §4.10a): a one-way propagation
/// delay with seeded jitter (in order per flow unless told it may reorder), an optional bottleneck
/// [`SimLink`] ahead of it, a [`SimLoss`] process, and an optional path MTU above which a datagram is
/// dropped (the black hole a path-MTU prober must find, RFC 8899). The fabric's default is the zero
/// path — delivered the instant it is sent, as the fabric always did — so a simulation that never sets
/// a profile runs unchanged. A profile is data describing the network under test, never a mode of the
/// code that runs over it (R8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimPath {
  /// The one-way propagation delay of the path, nanoseconds.
  one_way_ns: u64,
  /// The half-width of the jitter, nanoseconds: each datagram's propagation is `one_way ± jitter`.
  jitter_ns: u64,
  /// Whether a datagram may overtake an earlier one on the same directed flow. `false` — an arrival is
  /// clamped to no earlier than the previous datagram's on that flow; `true` — the jitter alone decides.
  reorders: bool,
  /// The loss process.
  loss: SimLoss,
  /// The largest datagram the path carries; a larger one is dropped. `None` — no limit.
  mtu: Option<usize>,
  /// The bottleneck the path's datagrams are serialized through, if any.
  link: Option<SimLinkId>,
}

impl Default for SimPath {
  fn default() -> SimPath {
    SimPath::NONE
  }
}

impl SimPath {
  /// The zero path: delivered at once, nothing lost (the fabric's default).
  pub const NONE: SimPath = SimPath {
    one_way_ns: 0,
    jitter_ns: 0,
    reorders: false,
    loss: SimLoss::NONE,
    mtu: None,
    link: None,
  };

  /// A path of `one_way_ns ± jitter_ns` that keeps each flow in send order.
  pub const fn in_order(one_way_ns: u64, jitter_ns: u64) -> SimPath {
    SimPath {
      one_way_ns,
      jitter_ns,
      reorders: false,
      loss: SimLoss::NONE,
      mtu: None,
      link: None,
    }
  }

  /// A path of `one_way_ns ± jitter_ns` whose jitter may reorder a flow.
  pub const fn reordering(one_way_ns: u64, jitter_ns: u64) -> SimPath {
    SimPath {
      one_way_ns,
      jitter_ns,
      reorders: true,
      loss: SimLoss::NONE,
      mtu: None,
      link: None,
    }
  }

  /// This path, losing datagrams by `loss`.
  pub const fn with_loss(self, loss: SimLoss) -> SimPath {
    SimPath { loss, ..self }
  }

  /// This path, dropping every datagram longer than `mtu` bytes.
  pub const fn with_mtu(self, mtu: usize) -> SimPath {
    SimPath {
      mtu: Some(mtu),
      ..self
    }
  }

  /// This path, serialized through `link` before it propagates.
  pub const fn through(self, link: SimLinkId) -> SimPath {
    SimPath {
      link: Some(link),
      ..self
    }
  }

  /// The one-way delay this profile centres on.
  pub fn one_way_ns(&self) -> u64 {
    self.one_way_ns
  }

  /// The half-width of this profile's jitter.
  pub fn jitter_ns(&self) -> u64 {
    self.jitter_ns
  }

  /// One datagram's propagation time: `one_way − jitter + U[0, 2·jitter]` from `rng`, or exactly the
  /// one-way delay when there is no jitter (drawing nothing).
  fn draw(&self, rng: &mut Xorshift) -> u64 {
    if self.jitter_ns == 0 {
      return self.one_way_ns;
    }
    let span = self.jitter_ns.saturating_mul(2).saturating_add(1);
    let offset = u64::try_from(rng.below(usize::try_from(span).unwrap_or(usize::MAX))).unwrap_or(0);
    self
      .one_way_ns
      .saturating_sub(self.jitter_ns)
      .saturating_add(offset)
  }
}

/// A NAT in front of a fabric port (RFC 4787's endpoint-independent mapping): the port's outbound
/// datagrams leave from an external port the NAT allocates; inbound datagrams to that external port reach
/// the inside port only while the mapping is alive — refreshed by each outbound datagram and expired
/// after `idle_timeout_ns` without one. The next outbound datagram after an expiry allocates a fresh
/// external port: the address change a peer sees as a NAT rebinding (RFC 9000 §9.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimNat {
  /// How long a mapping survives without outbound traffic, nanoseconds.
  pub idle_timeout_ns: u64,
}

/// A NAT's running state for one inside port.
#[derive(Debug)]
struct NatState {
  nat: SimNat,
  /// The current external port and the time of the last outbound datagram through it.
  mapping: Option<(u16, u64)>,
}

/// What the fabric did with the datagrams sent on it — the non-vacuity counters of every stress
/// scenario (a test of loss recovery asserts `dropped_loss` moved, of congestion `dropped_queue`, of
/// path-MTU discovery `dropped_mtu`, of migration `dropped_nat`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SimFabricStats {
  /// Datagrams handed to a receiver's mailbox.
  pub delivered: u64,
  /// Datagrams dropped because a link's queue was full.
  pub dropped_queue: u64,
  /// Datagrams dropped by a path's loss process.
  pub dropped_loss: u64,
  /// Datagrams dropped for exceeding a path's MTU.
  pub dropped_mtu: u64,
  /// Datagrams dropped at a NAT whose mapping had expired or never existed.
  pub dropped_nat: u64,
  /// Datagrams dropped because the receiver's buffer was full.
  pub dropped_receive_buffer: u64,
  /// Datagrams addressed to a port no socket is bound to.
  pub dropped_unbound: u64,
  /// The largest backlog any link's queue held, in bytes.
  pub peak_queue_bytes: u64,
}

/// A bound port's receive queue: the datagrams (bytes, source port) and the bytes they hold, bounded by
/// [`SIM_RECV_BUFFER_BYTES`].
#[derive(Debug, Default)]
struct Mailbox {
  queue: VecDeque<(Vec<u8>, u16)>,
  held: usize,
}

/// A datagram the fabric holds until its arrival time.
#[derive(Debug)]
struct InFlight {
  dest: u16,
  bytes: Vec<u8>,
  from: u16,
}

/// The simulated UDP fabric (§4.10a): a deterministic, in-memory datagram network so the fleet plane
/// is testable at N=1 without the OS network — the "sim arm first" the design's phasing calls for.
/// It is a thread-local because the simulation runs on one thread (so no `Send`/`Sync`, no lock), and
/// wakes a waiting receiver through the registry, the same path a real driver completion takes.
///
/// Every send is timed against the simulation clock the runtime installs ([`sim_fabric_reset`]) and the
/// directed pair's [`SimPath`] (else the fabric-wide one): a datagram is dropped past the path MTU, is
/// serialized through its bottleneck link (dropped if the link's queue is full), is lost by the path's
/// loss process, then propagates. A datagram whose arrival is now goes straight to its mailbox; a later
/// one waits in flight, ordered by arrival, and the simulation loop hands it over — and wakes its receiver
/// — once the clock reaches it ([`SimRuntime::run_until_idle`] also treats the earliest arrival as a
/// deadline the clock may advance to). A receiver's mailbox holds at most [`SIM_RECV_BUFFER_BYTES`], the
/// bound a kernel socket buffer has, and drops past it. With no clock installed "now" is zero.
#[derive(Debug)]
pub struct SimFabric {
  next_port: u16,
  /// Each bound port's queued datagrams.
  mailboxes: BTreeMap<u16, Mailbox>,
  interests: BTreeMap<u16, u64>,
  /// The seeded generator the jitter and loss are drawn from — the fabric's own stream, so a profile's
  /// draws never perturb the shards' generators and a run replays exactly from its seed.
  rng: Xorshift,
  /// The path of every directed pair without an override.
  default_path: SimPath,
  /// Directed overrides, keyed `(from, dest)` by the sockets' own ports.
  pair_paths: BTreeMap<(u16, u16), SimPath>,
  /// The bottleneck links, by id.
  links: Vec<LinkState>,
  /// Each directed flow's Gilbert–Elliott state: `true` while in the bad (burst) state.
  loss_state: BTreeMap<(u16, u16), bool>,
  /// NATs by inside port, and the current external→inside mappings.
  nats: BTreeMap<u16, NatState>,
  external: BTreeMap<u16, u16>,
  /// Every external port a NAT has allocated, live or expired, so a datagram to an expired mapping is
  /// counted as the NAT's drop rather than a closed port's. Bounded by the 16-bit port space.
  nat_ports: std::collections::BTreeSet<u16>,
  /// Datagrams not yet arrived, keyed by arrival time then send sequence (so two arrivals at one instant
  /// keep send order and never collide).
  in_flight: BTreeMap<(u64, u64), InFlight>,
  next_sequence: u64,
  /// The latest arrival scheduled on each directed flow, the floor an in-order path clamps the next to.
  last_arrival: BTreeMap<(u16, u16), u64>,
  stats: SimFabricStats,
}

/// Format: the salt that separates the fabric's generator stream from the shards' (both are seeded from
/// the runtime seed; `xorshift64*` seeded identically would draw identical words), a fixed odd word.
const FABRIC_SEED_SALT: u64 = 0xD1B5_4A32_D192_ED03;

impl SimFabric {
  fn new(seed: u64) -> SimFabric {
    SimFabric {
      // Ports start at 1 so 0 stays the "unspecified" address, as in the OS.
      next_port: 1,
      mailboxes: BTreeMap::new(),
      interests: BTreeMap::new(),
      rng: Xorshift::new(seed ^ FABRIC_SEED_SALT),
      default_path: SimPath::NONE,
      pair_paths: BTreeMap::new(),
      links: Vec::new(),
      loss_state: BTreeMap::new(),
      nats: BTreeMap::new(),
      external: BTreeMap::new(),
      nat_ports: std::collections::BTreeSet::new(),
      in_flight: BTreeMap::new(),
      next_sequence: 0,
      last_arrival: BTreeMap::new(),
      stats: SimFabricStats::default(),
    }
  }

  fn bind(&mut self) -> u16 {
    let port = self.next_port;
    self.next_port = self.next_port.saturating_add(1);
    self.mailboxes.entry(port).or_default();
    port
  }

  /// Virtual now: the clock of the shard sending on this fabric, or zero off a shard.
  fn now_ns(&self) -> u64 {
    crate::futures::now_ns()
  }

  /// The source port a datagram from `from` carries on the wire: `from` itself, or its NAT's current
  /// external port — allocating a fresh one when the mapping is absent or expired (a rebinding), and
  /// refreshing the mapping either way.
  fn translate_outbound(&mut self, from: u16, now: u64) -> u16 {
    let Some((idle_timeout_ns, mapping)) = self
      .nats
      .get(&from)
      .map(|state| (state.nat.idle_timeout_ns, state.mapping))
    else {
      return from;
    };
    let alive = mapping
      .filter(|(_, last)| now.saturating_sub(*last) <= idle_timeout_ns)
      .map(|(port, _)| port);
    let port = match alive {
      Some(port) => port,
      None => {
        if let Some((old, _)) = mapping {
          self.external.remove(&old);
        }
        let fresh = self.bind();
        self.mailboxes.remove(&fresh);
        self.external.insert(fresh, from);
        self.nat_ports.insert(fresh);
        fresh
      }
    };
    if let Some(state) = self.nats.get_mut(&from) {
      state.mapping = Some((port, now));
    }
    port
  }

  /// The inside port a datagram addressed to `dest` reaches at `now`: `dest` itself when no NAT owns
  /// it, the inside port behind a live mapping, or `None` when it names an expired or unknown mapping.
  fn translate_inbound(&self, dest: u16, now: u64) -> Option<u16> {
    let Some(&inside) = self.external.get(&dest) else {
      // An inside port is reachable only through its NAT, and an expired external port reaches nothing.
      return (!self.nats.contains_key(&dest) && !self.nat_ports.contains(&dest)).then_some(dest);
    };
    let state = self.nats.get(&inside)?;
    let (port, last) = state.mapping?;
    (port == dest && now.saturating_sub(last) <= state.nat.idle_timeout_ns).then_some(inside)
  }

  /// The path a datagram from `from` to `dest` takes (both the sockets' own ports).
  fn path(&self, from: u16, dest: u16) -> SimPath {
    self
      .pair_paths
      .get(&(from, dest))
      .copied()
      .unwrap_or(self.default_path)
  }

  /// Whether the flow's loss process drops the next datagram, advancing its Gilbert–Elliott state.
  fn lose(&mut self, flow: (u16, u16), loss: SimLoss) -> bool {
    if loss.is_lossless() {
      return false;
    }
    let bad = self.loss_state.get(&flow).copied().unwrap_or(false);
    let next = if bad {
      !chance(&mut self.rng, loss.bad_to_good_ppm)
    } else {
      chance(&mut self.rng, loss.good_to_bad_ppm)
    };
    self.loss_state.insert(flow, next);
    chance(
      &mut self.rng,
      if next {
        loss.bad_loss_ppm
      } else {
        loss.good_loss_ppm
      },
    )
  }

  /// Sends a datagram from socket `from` to address port `dest`: returns the waker word of a receiver to
  /// wake when it is delivered at once, or schedules (or drops) it per the path.
  fn send(&mut self, dest: u16, bytes: &[u8], from: u16) -> Option<u64> {
    let now = self.now_ns();
    let wire_from = self.translate_outbound(from, now);
    // The path is chosen by the two sockets, so a scenario configures it by the ports it bound; a
    // datagram to a NAT's external port follows the path to the inside port behind it.
    let path_dest = self.external.get(&dest).copied().unwrap_or(dest);
    let path = self.path(from, path_dest);
    if path.mtu.is_some_and(|mtu| bytes.len() > mtu) {
      self.stats.dropped_mtu = self.stats.dropped_mtu.saturating_add(1);
      return None;
    }
    let mut departure = now;
    if let Some(SimLinkId(index)) = path.link
      && let Some(link) = self
        .links
        .get_mut(usize::try_from(index).unwrap_or(usize::MAX))
    {
      let backlog = link.backlog_bytes(now);
      if backlog.saturating_add(bytes.len() as u64) > link.link.queue_bytes {
        self.stats.dropped_queue = self.stats.dropped_queue.saturating_add(1);
        return None;
      }
      self.stats.peak_queue_bytes = self.stats.peak_queue_bytes.max(backlog);
      departure = link
        .busy_until_ns
        .max(now)
        .saturating_add(link.serialization_ns(bytes.len()));
      link.busy_until_ns = departure;
    }
    if self.lose((from, path_dest), path.loss) {
      self.stats.dropped_loss = self.stats.dropped_loss.saturating_add(1);
      return None;
    }
    let mut arrival = departure.saturating_add(path.draw(&mut self.rng));
    if !path.reorders
      && let Some(previous) = self.last_arrival.get(&(from, path_dest))
    {
      arrival = arrival.max(*previous);
    }
    self.last_arrival.insert((from, path_dest), arrival);
    if arrival <= now {
      return self.arrive(dest, bytes.to_vec(), wire_from, now);
    }
    let sequence = self.next_sequence;
    self.next_sequence = self.next_sequence.saturating_add(1);
    self.in_flight.insert(
      (arrival, sequence),
      InFlight {
        dest,
        bytes: bytes.to_vec(),
        from: wire_from,
      },
    );
    None
  }

  /// A datagram reaching address port `dest` at `now`: through a NAT's live mapping to its inside port,
  /// into the receiver's mailbox within its buffer bound; returns the waker word of a waiting receiver.
  fn arrive(&mut self, dest: u16, bytes: Vec<u8>, from: u16, now: u64) -> Option<u64> {
    let Some(inside) = self.translate_inbound(dest, now) else {
      self.stats.dropped_nat = self.stats.dropped_nat.saturating_add(1);
      return None;
    };
    let Some(Mailbox { queue, held }) = self.mailboxes.get_mut(&inside) else {
      // No socket is bound there: the datagram is dropped, as the OS drops one to a closed port.
      self.stats.dropped_unbound = self.stats.dropped_unbound.saturating_add(1);
      return None;
    };
    if held.saturating_add(bytes.len()) > SIM_RECV_BUFFER_BYTES {
      self.stats.dropped_receive_buffer = self.stats.dropped_receive_buffer.saturating_add(1);
      return None;
    }
    *held = held.saturating_add(bytes.len());
    queue.push_back((bytes, from));
    self.stats.delivered = self.stats.delivered.saturating_add(1);
    self.interests.remove(&inside)
  }

  /// Hands over every in-flight datagram whose arrival is at or before `now`, in arrival order, and
  /// returns the waker words of the receivers waiting on them.
  fn deliver_due(&mut self, now: u64) -> Vec<u64> {
    let mut wakes = Vec::new();
    while let Some(entry) = self.in_flight.first_entry() {
      if entry.key().0 > now {
        break;
      }
      let arrival = entry.key().0;
      let InFlight { dest, bytes, from } = entry.remove();
      if let Some(word) = self.arrive(dest, bytes, from, arrival) {
        wakes.push(word);
      }
    }
    wakes
  }

  /// The earliest arrival still in flight, if any — a deadline the simulation clock may advance to.
  fn earliest_arrival(&self) -> Option<u64> {
    self.in_flight.keys().next().map(|(arrival, _)| *arrival)
  }

  fn recv(&mut self, port: u16) -> Option<(Vec<u8>, u16)> {
    let mailbox = self.mailboxes.get_mut(&port)?;
    let (bytes, from) = mailbox.queue.pop_front()?;
    mailbox.held = mailbox.held.saturating_sub(bytes.len());
    Some((bytes, from))
  }

  /// Records one-shot read interest; returns a waker word to wake now if a datagram already waits.
  fn register(&mut self, port: u16, word: u64) -> Option<u64> {
    if self
      .mailboxes
      .get(&port)
      .is_some_and(|mailbox| !mailbox.queue.is_empty())
    {
      return Some(word);
    }
    self.interests.insert(port, word);
    None
  }
}

thread_local! {
  static SIM_FABRIC: RefCell<SimFabric> = RefCell::new(SimFabric::new(0));
}

/// Resets the thread's simulated UDP fabric for a fresh simulation: an empty network on the zero path,
/// drawing its jitter and loss from `seed`.
pub(crate) fn sim_fabric_reset(seed: u64) {
  SIM_FABRIC.with(|f| *f.borrow_mut() = SimFabric::new(seed));
}

/// Sets the path of every directed pair on this thread's fabric that has no override — the whole
/// modelled network at one profile. Takes effect for datagrams sent from now on; call it after
/// `SimRuntime::new` (which resets the fabric) and before the tasks that send.
pub fn sim_udp_set_path(path: SimPath) {
  SIM_FABRIC.with(|f| f.borrow_mut().default_path = path);
}

/// Sets the path from socket port `from` to socket port `dest`, overriding the fabric's default for that
/// directed pair only — a near pair inside a far fleet, an asymmetric route, or one flow's bottleneck.
pub fn sim_udp_set_pair_path(from: u16, dest: u16, path: SimPath) {
  SIM_FABRIC.with(|f| {
    f.borrow_mut().pair_paths.insert((from, dest), path);
  });
}

/// Adds a bottleneck link to this thread's fabric; paths name it with [`SimPath::through`].
pub fn sim_udp_add_link(link: SimLink) -> SimLinkId {
  SIM_FABRIC.with(|f| {
    let mut fabric = f.borrow_mut();
    let id = SimLinkId(u32::try_from(fabric.links.len()).unwrap_or(u32::MAX));
    fabric.links.push(LinkState {
      link,
      busy_until_ns: 0,
    });
    id
  })
}

/// Changes a link's rate and queue from now on (a path whose capacity drops or recovers mid-run — the
/// variable-bandwidth case a controller must follow); the backlog already queued drains at the old
/// timing.
pub fn sim_udp_set_link(id: SimLinkId, link: SimLink) {
  SIM_FABRIC.with(|f| {
    if let Some(state) = f
      .borrow_mut()
      .links
      .get_mut(usize::try_from(id.0).unwrap_or(usize::MAX))
    {
      state.link = link;
    }
  });
}

/// Puts a NAT in front of socket port `inside`: its datagrams leave from an external port that expires
/// after `nat.idle_timeout_ns` without outbound traffic.
pub fn sim_udp_set_nat(inside: u16, nat: SimNat) {
  SIM_FABRIC.with(|f| {
    f.borrow_mut()
      .nats
      .insert(inside, NatState { nat, mapping: None });
  });
}

/// Expires the NAT mapping in front of `inside` now, so its next datagram leaves from a fresh external
/// port — an address change at a moment the scenario chooses (a Wi-Fi to cellular move, a NAT reboot).
pub fn sim_udp_rebind(inside: u16) {
  SIM_FABRIC.with(|f| {
    let mut fabric = f.borrow_mut();
    let old = fabric
      .nats
      .get_mut(&inside)
      .and_then(|state| state.mapping.take());
    if let Some((port, _)) = old {
      fabric.external.remove(&port);
    }
  });
}

/// What the fabric has done so far (delivered and dropped datagrams by cause, the peak queue).
pub fn sim_udp_stats() -> SimFabricStats {
  SIM_FABRIC.with(|f| f.borrow().stats)
}

/// Hands over every in-flight datagram due by `now` and wakes the receivers waiting on them.
fn sim_fabric_deliver_due(now: u64) {
  let wakes = SIM_FABRIC.with(|f| f.borrow_mut().deliver_due(now));
  for word in wakes {
    crate::registry::wake(Encoded::from_word(word));
  }
}

/// The earliest arrival still in flight on this thread's fabric.
fn sim_fabric_earliest_arrival() -> Option<u64> {
  SIM_FABRIC.with(|f| f.borrow().earliest_arrival())
}

/// Shape: the receive buffer of a simulated datagram socket — what its mailbox holds before dropping, and
/// what `UdpSocket::recv_buffer_bytes` reports — the smaller of the default kernel datagram buffers on the
/// machines this runs on (Linux 208 KiB, macOS 768 KiB), so a consumer is sized and stressed as it would
/// be on the stricter host.
pub const SIM_RECV_BUFFER_BYTES: usize = 208 * 1024;

/// Binds a simulated UDP port on this thread's fabric.
pub fn sim_udp_bind() -> u16 {
  SIM_FABRIC.with(|f| f.borrow_mut().bind())
}

/// Sends a simulated datagram along its path: delivered at once on the zero path (a waiting receiver is
/// woken through the registry), scheduled for its arrival on a delayed one, or dropped as the path says.
pub fn sim_udp_send(dest: u16, bytes: &[u8], from: u16) {
  let wake = SIM_FABRIC.with(|f| f.borrow_mut().send(dest, bytes, from));
  if let Some(word) = wake {
    crate::registry::wake(Encoded::from_word(word));
  }
}

/// Receives one simulated datagram, or `None` when the mailbox is empty.
pub fn sim_udp_recv(port: u16) -> Option<(Vec<u8>, u16)> {
  SIM_FABRIC.with(|f| f.borrow_mut().recv(port))
}

/// Registers one-shot read interest, waking now (through the registry) if a datagram already waits.
pub fn sim_udp_register(port: u16, word: u64) {
  let wake = SIM_FABRIC.with(|f| f.borrow_mut().register(port, word));
  if let Some(word) = wake {
    crate::registry::wake(Encoded::from_word(word));
  }
}

/// The state a simulated driver shares with its kick and the simulation clock.
#[derive(Debug)]
pub struct SimShared {
  now_ns: AtomicU64,
  kicked: AtomicBool,
  requested_deadline: AtomicU64,
  rng_state: AtomicU64,
  kill_at_wait: AtomicU64,
  waits: AtomicU64,
}

impl SimShared {
  fn new(seed: u64) -> Self {
    Self {
      now_ns: AtomicU64::new(0),
      kicked: AtomicBool::new(false),
      requested_deadline: AtomicU64::new(NO_DEADLINE),
      rng_state: AtomicU64::new(Xorshift::new(seed).next_u64()),
      kill_at_wait: AtomicU64::new(NO_DEADLINE),
      waits: AtomicU64::new(0),
    }
  }

  /// Marks the driver kicked.
  pub fn set_kicked(&self) {
    self.kicked.store(true, Ordering::Release);
  }

  /// Virtual now.
  pub fn now_ns(&self) -> u64 {
    self.now_ns.load(Ordering::Acquire)
  }

  /// The next pseudo-random word from the simulation's seeded generator.
  pub fn next_random(&self) -> u64 {
    let mut rng = Xorshift::new(self.rng_state.load(Ordering::Acquire));
    let value = rng.next_u64();
    self.rng_state.store(value, Ordering::Release);
    value
  }

  fn requested_deadline(&self) -> Option<u64> {
    match self.requested_deadline.load(Ordering::Acquire) {
      NO_DEADLINE => None,
      d => Some(d),
    }
  }
}

/// The simulation driver of one shard.
#[derive(Debug)]
pub struct SimDriver {
  kick: Kick,
  shared: &'static SimShared,
  clock: &'static SimShared,
  nops: Vec<u64>,
}

impl Driver for SimDriver {
  fn kind(&self) -> DriverKind {
    DriverKind::Simulation
  }

  fn kick_handle(&self) -> Kick {
    self.kick
  }

  fn now_ns(&self) -> u64 {
    self.clock.now_ns()
  }

  fn wait(&mut self, timeout_ns: Option<u64>, out: &mut Vec<Completion>) -> Result<(), RtError> {
    let waits = self.shared.waits.fetch_add(1, Ordering::AcqRel) + 1;
    if waits >= self.shared.kill_at_wait.load(Ordering::Acquire) {
      return Err(RtError::DriverLost);
    }
    out.extend(self.nops.drain(..).map(|user_data| Completion {
      user_data,
      result: 0,
    }));
    if self.shared.kicked.swap(false, Ordering::AcqRel) || !out.is_empty() {
      self
        .shared
        .requested_deadline
        .store(NO_DEADLINE, Ordering::Release);
      return Ok(());
    }
    // Nothing to deliver: record the deadline for the simulation clock and return without
    // blocking; the runtime advances time when every shard is idle.
    let deadline = timeout_ns.map_or(NO_DEADLINE, |t| self.now_ns().saturating_add(t));
    self
      .shared
      .requested_deadline
      .store(deadline, Ordering::Release);
    Ok(())
  }

  fn submit_nop(&mut self, user_data: u64) -> Result<(), RtError> {
    self.nops.push(user_data);
    Ok(())
  }

  fn register_readable(&mut self, raw: i32, user_data: u64) -> Result<(), RtError> {
    // The simulated UDP fabric (§4.10a): the raw handle is a sim port; interest is recorded there and
    // woken through the registry when a datagram arrives — the same wake path a real completion takes.
    let port = u16::try_from(raw).map_err(|_| RtError::DriverRefused {
      call: "register_readable",
      code: None,
    })?;
    sim_udp_register(port, user_data);
    Ok(())
  }

  fn register_writable(&mut self, _raw: i32, _user_data: u64) -> Result<(), RtError> {
    // The simulated fabric is UDP-only and its sends never block (an in-memory push), so there is no
    // write-readiness to await under simulation; TCP is a host-local bridge (§4.6), not the fleet
    // plane (§4.10a). A typed refusal keeps the seam honest rather than pretending readiness.
    Err(RtError::DriverRefused {
      call: "register_writable",
      code: None,
    })
  }

  fn has_pending(&self) -> bool {
    !self.nops.is_empty() || self.shared.kicked.load(Ordering::Acquire)
  }

  fn is_sim(&self) -> bool {
    true
  }
}

/// A set of simulated shards on the calling thread.
pub struct SimRuntime {
  shards: Vec<&'static ShardContext>,
  /// The simulation clock, read here and borrowed by the drivers for the life of their contexts.
  clock: &'static SimShared,
  /// The clock's allocation, owned here as a raw pointer and freed in `Drop` after every context is
  /// reclaimed. A raw pointer, not a `Box`: moving a `Box` (into this struct, or this struct out of
  /// `new`) is a unique retag of its allocation under Stacked Borrows, which invalidated the shared
  /// borrows the drivers already held — Miri caught the drivers' next `now_ns` reading through a tag
  /// no longer on the borrow stack (2026-09-16; `-p slates-rt --test differential`).
  clock_allocation: std::ptr::NonNull<SimShared>,
  shared: Vec<&'static SimShared>,
}

impl std::fmt::Debug for SimRuntime {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("SimRuntime")
      .field("shards", &self.shards.len())
      .finish()
  }
}

impl Drop for SimRuntime {
  /// The simulation's shards were built and stepped on this thread; every `run_until_*` returned
  /// before this drop. Each context is freed (`reclaim_context`, which also clears the thread's
  /// current-context cell a bare step left pointing at it) and its slot given back — before this, a
  /// simulation reclaimed nothing (its slots were never unregistered, so a test binary spent one of
  /// the registry's slots per simulation for good). The per-shard flags are the slots' own (a retired
  /// entry is reclaimed after the last counted kick borrow), and the clock is this runtime's
  /// allocation, dropped after the contexts.
  fn drop(&mut self) {
    let ids: Vec<_> = self.shards.iter().map(|context| context.id).collect();
    for ctx in &self.shards {
      crate::registry::note_arena_generation(ctx.id, ctx.arena_generation_high());
      crate::registry::reclaim_context(ctx.id);
    }
    // Cancellation can wake another shard. Keep every pair ring until all contexts ended.
    for id in ids {
      crate::registry::unregister(id);
    }
    // SAFETY: the allocation was made by `Box::new` in `new` and is freed exactly once, here, after
    // every context — and so every driver holding a `&'static` into it — was reclaimed above; nothing
    // reads `self.clock` after this.
    unsafe { drop(Box::from_raw(self.clock_allocation.as_ptr())) };
  }
}

impl SimRuntime {
  /// Builds `config.shards` simulated shards sharing one clock seeded by `seed`.
  pub fn new(config: &RuntimeConfig, seed: u64) -> Result<SimRuntime, RtError> {
    let clock_allocation = std::ptr::NonNull::from(Box::leak(Box::new(SimShared::new(seed))));
    // SAFETY: the allocation lives until this runtime's `Drop` frees it, after every context (and so
    // every driver borrowing it) was reclaimed, so no driver outlives the clock; and it is never moved
    // through a `Box` again — the pointer is what the struct holds — so no unique retag invalidates
    // these shared borrows while they are live. Before 2026-09-14 the clock was leaked per simulation;
    // from then until 2026-09-16 it was a `Box` field, whose move invalidated the borrows (Miri).
    let clock: &'static SimShared = unsafe { clock_allocation.as_ref() };
    // A fresh simulation starts with an empty UDP fabric on this thread.
    sim_fabric_reset(seed);
    let mut seeds = Vec::new();
    let mut shared = Vec::new();
    for _ in 0..config.shards {
      // The driver takes its flags from the kick the slot minted over them (the flags are the slot's,
      // so a stale waker may still kick them after this simulation ends; see `registry::Entry`).
      let driver: DriverSeed = Box::new(move |kick| match kick {
        Kick::Sim(holder) => {
          // The owning thread borrows the entry until its context is reclaimed. Foreign
          // kicks carry only the holder and borrow under the registry's reader pin.
          let flags = crate::registry::entry(holder.shard())
            .and_then(|entry| entry.sim_shared.as_deref())
            .ok_or(RtError::ShardGone {
              shard: holder.shard(),
            })?;
          Ok(Box::new(SimDriver {
            kick,
            shared: flags,
            clock,
            nops: Vec::new(),
          }) as Box<dyn Driver>)
        }
        _ => Err(RtError::DriverRefused {
          call: "a simulated shard registered without simulated flags",
          code: None,
        }),
      });
      let registered = ShardSeed::register(
        config,
        driver,
        crate::registry::RegisterKick::Sim(Box::new(SimShared::new(seed))),
      )?;
      let flags = crate::registry::entry(registered.id)
        .and_then(|entry| entry.sim_shared.as_deref())
        .ok_or(RtError::ShardGone {
          shard: registered.id,
        })?;
      shared.push(flags);
      seeds.push(registered);
    }
    crate::runtime::connect_pairs(&mut seeds)?;
    let shards = seeds
      .into_iter()
      .map(ShardContext::build)
      .collect::<Result<Vec<_>, _>>()?;
    Ok(SimRuntime {
      shards,
      clock,
      clock_allocation,
      shared,
    })
  }

  /// The shard ids, in order.
  pub fn shard_ids(&self) -> Vec<ShardId> {
    self.shards.iter().map(|s| ShardId(s.id)).collect()
  }

  /// Virtual now.
  pub fn now_ns(&self) -> u64 {
    self.clock.now_ns()
  }

  /// Spawns a detached task on a shard.
  pub fn spawn_on(
    &mut self,
    shard: ShardId,
    future: impl std::future::Future<Output = ()> + Send + 'static,
  ) -> Result<TaskId, RtError> {
    let ctx = self.context(shard)?;
    ctx.spawn_request(SpawnRequest::new(Box::pin(future), None))
  }

  /// Makes a shard's driver fail at its `nth` wait from now (1 = the very next one).
  pub fn kill_driver(&mut self, shard: ShardId, nth: u64) -> Result<(), RtError> {
    let index = self.index_of(shard)?;
    let shared = self.shared[index];
    shared.kill_at_wait.store(
      shared.waits.load(Ordering::Acquire) + nth,
      Ordering::Release,
    );
    Ok(())
  }

  /// Steps every shard until none has work, advancing virtual time to the earliest deadline —
  /// a shard's timer or a datagram's arrival on the fabric — whenever all are idle. Datagrams due by
  /// the current time are handed over (and their receivers woken) before each pass, so a delayed
  /// datagram is received exactly at its arrival time. Returns the number of steps taken.
  pub fn run_until_idle(&mut self) -> u64 {
    let mut steps = 0u64;
    loop {
      sim_fabric_deliver_due(self.clock.now_ns());
      let mut any_work = false;
      let mut earliest: Option<u64> = None;
      for index in 0..self.shards.len() {
        let ctx = self.shards[index];
        if ctx.exited() {
          continue;
        }
        let outcome: StepOutcome = ctx.step();
        steps += 1;
        if outcome.did_work {
          any_work = true;
          continue;
        }
        ctx.park(outcome.next_deadline_ns);
        let shared = self.shared[index];
        if let Some(deadline) = shared.requested_deadline() {
          earliest = Some(earliest.map_or(deadline, |e| e.min(deadline)));
        }
        if shared.kicked.load(Ordering::Acquire) {
          any_work = true;
        }
      }
      if any_work {
        continue;
      }
      // A datagram in flight is a pending event too: the clock may advance to its arrival.
      if let Some(arrival) = sim_fabric_earliest_arrival() {
        earliest = Some(earliest.map_or(arrival, |e| e.min(arrival)));
      }
      match earliest {
        Some(deadline) => {
          if deadline > self.clock.now_ns() {
            self.clock.now_ns.store(deadline, Ordering::Release);
          }
        }
        None => break,
      }
    }
    steps
  }

  /// Advances the clock by `ns` without running anything (a pause in the story).
  pub fn advance(&mut self, ns: u64) {
    let now = self.clock.now_ns();
    self
      .clock
      .now_ns
      .store(now.saturating_add(ns), Ordering::Release);
  }

  /// A shard's context, for counters and joins in tests.
  pub fn context(&self, shard: ShardId) -> Result<&'static ShardContext, RtError> {
    let index = self.index_of(shard)?;
    self
      .shards
      .get(index)
      .copied()
      .ok_or(RtError::NotOnShardThread)
  }

  fn index_of(&self, shard: ShardId) -> Result<usize, RtError> {
    self
      .shards
      .iter()
      .position(|c| c.id == shard.0)
      .ok_or(RtError::NotOnShardThread)
  }
}
