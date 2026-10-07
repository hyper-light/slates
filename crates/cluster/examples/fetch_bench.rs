//! The remote pull benchmark (condition 7; §4.10 "chunk reads fault to hedged fetches by identity from the recorded
//! holders"; A-91): readers fetch a sealed archive's chunks from its recorded holders over real endpoints — TLS 1.3,
//! packet protection, the session plane's congestion controller — on the simulated network (`crates/rt/src/sim.rs`),
//! across delay, loss, a degraded holder, a silent holder and many readers at once.
//!
//! Topology: every holder sends through its own bottleneck link (its uplink, shared by every reader it serves); the
//! reverse paths carry only requests. Each reader holds one session per holder, fetches the manifest from the first,
//! then the chunks through `fetch_chunks`, striped across the holders and hedged at the measured p95: a warm-up fetch
//! of the same archive records each chunk's latency, and the timed fetch hedges at that window's p95, as the daemon's
//! fetch class does once it has readings. Every reader's rebuilt archive is compared byte for byte with the original.
//!
//! `cargo run --release -p slates-cluster --example fetch_bench [scenario-filter] [chunks]` prints one row per scenario: the
//! readers' completion times (virtual, from the fetch's start), the aggregate goodput against the holders' summed
//! uplink capacity, the hedges sent and the holders dropped. Deterministic from the simulation's virtual clock.
//! **Failures** (the process exits non-zero): any reader incomplete or any archive rebuilt wrong.

// Benchmark harness: an unwrap here is a failed run, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::cast_precision_loss,
  clippy::cast_possible_truncation,
  clippy::cast_sign_loss,
  clippy::too_many_lines,
  clippy::indexing_slicing
)]

use std::cell::RefCell;
use std::sync::mpsc::{Receiver, Sender, channel};

use rustls::pki_types::PrivateKeyDer;
use slates_archive::{Archive, Entry, Extent, Node, NodeMeta};
use slates_cluster::content::{ContentHold, FetchTiming, HoldSpace, Placed, fetch};
use slates_db::register::{HostId, ObjectId};
use slates_mem::arena::ChunkArena;
use slates_mem::budget::{MetadataBudget, ShardBudget};
use slates_rt::runtime::RuntimeConfig;
use slates_rt::shard::Kept;
use slates_rt::sim::{
  PPM, SimLink, SimLoss, SimPath, SimRuntime, sim_udp_add_link, sim_udp_set_pair_path,
};
use slates_rt::udp::{Ipv4Addr, SocketAddrV4, UdpSocket};
use slates_transport::connection::{ConnectionShape, initial_receive_window};
use slates_transport::endpoint::{Endpoint, MAX_PACKET_PAYLOAD, MIN_DATAGRAM_BYTES};
use slates_transport::handshake::Identity;

/// Shape: the archive's chunks, and each chunk's bytes: 4 MiB in 64 KiB chunks, enough chunks that a fetch stripes
/// and pipelines across three holders, small enough that a grid of scenarios runs in seconds.
const DEFAULT_CHUNKS: usize = 64;

/// The archive's chunk count for this run: the second argument, or [`DEFAULT_CHUNKS`] (4 MiB). A larger object shows
/// the steady state a 4 MiB one hides behind the manifest's round trip and slow start.
static CHUNK_COUNT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

fn chunks() -> usize {
  *CHUNK_COUNT.get_or_init(|| {
    std::env::args()
      .nth(2)
      .and_then(|count| count.parse().ok())
      .unwrap_or(DEFAULT_CHUNKS)
  })
}
/// Shape: one chunk's bytes (see [`chunks`]).
const CHUNK_BYTES: usize = 64 * 1024;
/// Shape: the enrolled server name every test certificate carries.
const NAME: &str = "slates-node";
/// Format: bits per byte.
const BITS_PER_BYTE: u64 = 8;
/// Format: nanoseconds per second.
const NS_PER_SECOND: u64 = 1_000_000_000;
/// Format: nanoseconds per millisecond.
const NS_PER_MS: u64 = 1_000_000;
/// Shape: the coordinator's and each worker's poll: a tenth of a millisecond, finer than any path's round trip
/// here, so polling adds no visible latency.
const POLL_NS: u64 = 100_000;
/// Shape: the fetch's whole span: generous against the slowest scenario (the thin link: 64 MiB over 10 Mbit/s is
/// 54 s of transfer alone, before a 200 ms round trip, 2% loss and reordering), so the deadline never decides a
/// completion time; a reader still incomplete at it is a failure.
const DEADLINE_NS: u64 = 600 * NS_PER_SECOND;
/// Shape: a queue ahead of each holder's uplink of one bandwidth-delay product, the classic router sizing
/// (Villamizar & Song 1994), at least a few datagrams.
const MIN_QUEUE_BYTES: u64 = 16 * MIN_DATAGRAM_BYTES as u64;
/// Format: the owner of the fetched object (its id names it, nothing more here).
const OWNER: HostId = HostId(1);
/// Shape: the hedge with no latency readings yet, the daemon's own (`fleet::fetch_into_hold` falls back to its
/// heartbeat, 100 ms). The warm-up once hedged at the deadline instead, so a silent holder ranked first for the
/// manifest held the warm-up to it and the timed fetch never learned a p95 (seen 2026-10-05 once distinct chunks
/// changed the archive's identity and so its holders' ranks).
const COLD_HEDGE_NS: u64 = 100 * NS_PER_MS;

/// One scenario of the grid.
#[derive(Clone, Debug)]
struct Scenario {
  name: &'static str,
  holders: usize,
  readers: usize,
  rtt_ns: u64,
  rate_bits_per_second: u64,
  loss: SimLoss,
  /// The half-width of each path's jitter: zero keeps every flow in order; above zero the jitter may reorder it (the
  /// horrific profiles: a datagram may arrive up to twice this after one sent later).
  jitter_ns: u64,
  /// Whether the jitter keeps each flow in send order (varying delay alone, no reordering).
  jitter_in_order: bool,
  /// A holder index whose uplink runs at `rate / slow_divisor` (a degraded holder), if any.
  slow: Option<(usize, u64)>,
  /// A holder index whose paths drop everything once the sessions formed (a silent holder), if any.
  silent: Option<usize>,
}

impl Scenario {
  fn bdp_bytes(&self) -> u64 {
    self.rate_bits_per_second / BITS_PER_BYTE * self.rtt_ns / NS_PER_SECOND
  }

  /// Each session's receive ceiling: twice the path's bandwidth-delay product, never less than the initial
  /// window — the window a session auto-tuned to its path reaches (the daemon bounds it by a share of the
  /// shard's reserve, `DaemonConfig::fleet_session_receive_bytes`).
  fn receive_ceiling(&self) -> u64 {
    (self.bdp_bytes() * 2).max(initial_receive_window(MAX_PACKET_PAYLOAD))
  }

  fn holder_rate(&self, holder: usize) -> u64 {
    match self.slow {
      Some((slow, divisor)) if slow == holder => self.rate_bits_per_second / divisor,
      _ => self.rate_bits_per_second,
    }
  }
}

/// What one scenario measured.
struct Outcome {
  completions_ns: Vec<u64>,
  hedges: u64,
  steals: u64,
  failed_holders: u64,
  all_rebuilt: bool,
  /// The holders' declared losses the readers then acknowledged, summed ([`SenderStats`]).
  spurious_losses: u64,
  /// The smallest holder congestion window at the end, bytes.
  min_congestion_window: u64,
}

fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 1,
    tasks_per_shard: 512,
    timers_per_shard: 512,
    ring_entries: 64,
    step_budget_ns: 1_000_000_000,
    timer_tick_ns: POLL_NS,
    batch: 64,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
    wake_tracking: None,
  }
}

fn identity() -> Identity {
  // Ed25519: a signature is always 64 bytes, so the handshake's packets are the same size every run; an ECDSA
  // signature's DER length varies with its random nonce (70-72 bytes), which moved packet boundaries and so the
  // virtual timings, and some rows differed run to run.
  let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
  let certificate = rcgen::CertificateParams::new(vec![NAME.to_owned()])
    .unwrap()
    .self_signed(&key)
    .unwrap();
  Identity::from_der(
    certificate.der().clone(),
    PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
  )
}

/// The archive every holder holds: [`chunks`] files of [`CHUNK_BYTES`], each its own chunk.
fn archive() -> Archive {
  let chunks: Vec<_> = (0..chunks())
    .map(|at| {
      let mut bytes = vec![0u8; CHUNK_BYTES];
      for (offset, byte) in bytes.iter_mut().enumerate() {
        *byte = (at.wrapping_mul(31) ^ offset.wrapping_mul(7)) as u8;
      }
      // The chunk's index in its first bytes, so every chunk is distinct: the pattern alone repeats every 256 chunks,
      // and a fetch moves one copy of each identity (a 1,024-chunk archive moved 16 MiB, not 64).
      bytes[..8].copy_from_slice(&(at as u64).to_le_bytes());
      Archive::raw_chunk(bytes)
    })
    .collect();
  let entries = chunks
    .iter()
    .enumerate()
    .map(|(at, chunk)| Entry {
      // Five digits, so the names sort in creation order up to 100,000 chunks (three broke past 999: `f1000` < `f101`).
      name: format!("f{at:05}"),
      meta: NodeMeta {
        size: chunk.raw_len,
        ..NodeMeta::default()
      },
      node: Node::File(vec![Extent {
        offset: 0,
        len: chunk.raw_len,
        chunk: chunk.identity,
        chunk_offset: 0,
      }]),
    })
    .collect();
  Archive {
    base_page_size: 4096,
    chunk_min: 4096,
    chunk_max: CHUNK_BYTES as u32,
    created_unix: 1,
    volume_id: 1,
    snapshot_id: 1,
    name_policy_id: 0,
    unicode_version: 0,
    root_meta: NodeMeta::default(),
    manifest: Node::Directory(entries),
    chunks,
  }
}

/// A shard memory big enough for the archive twice over (a hold and the verification scratch), a power of two of
/// pages for the buddy.
struct Room {
  arena: ChunkArena,
  budget: ShardBudget,
  metadata: MetadataBudget,
}

impl Room {
  fn new() -> Room {
    let page = rustix::param::page_size();
    let bytes = (chunks() * CHUNK_BYTES * 2).next_power_of_two();
    let mut arena = ChunkArena::new(page);
    arena
      .add_region(slates_mem::region::Region::map(bytes, page, false).unwrap())
      .unwrap();
    let capacity = u64::try_from(arena.capacity()).unwrap();
    Room {
      arena,
      budget: ShardBudget::new(capacity, 0),
      metadata: MetadataBudget::new(u64::MAX),
    }
  }

  fn space(&mut self) -> HoldSpace<'_> {
    HoldSpace {
      arena: &mut self.arena,
      budget: &mut self.budget,
      metadata: &mut self.metadata,
    }
  }
}

/// One holder's hold, kept on the shard so each of its sessions' serve tasks lends it in turn.
struct Holder {
  held: ContentHold,
  room: Room,
}

/// Waits for a value on `receiver`, yielding on the timer tick; the receiver comes back with it (owned across the
/// wait, so the waiting future stays `Send`).
async fn receive<T>(receiver: Receiver<T>) -> (T, Receiver<T>) {
  loop {
    if let Ok(value) = receiver.try_recv() {
      return (value, receiver);
    }
    slates_rt::futures::sleep(POLL_NS).await.unwrap();
  }
}

/// The nearest-rank 95th percentile of `readings` (the machine crate's percentile law), `None` when empty.
fn p95(readings: &[u64]) -> Option<u64> {
  let mut sorted = readings.to_vec();
  sorted.sort_unstable();
  let rank = (sorted.len() * 95).div_ceil(100);
  sorted.get(rank.checked_sub(1)?).copied()
}

/// Serves `endpoint`'s requests from the kept hold until `stop` says so.
async fn serve(
  mut endpoint: Endpoint,
  holder: Kept<RefCell<Holder>>,
  host: HostId,
  stop: Receiver<()>,
  stats: Sender<SenderStats>,
) {
  let report = |endpoint: &Endpoint| {
    let _ = stats.send(SenderStats {
      spurious_losses: endpoint.spurious_losses(),
      congestion_window: endpoint.congestion_window(),
    });
  };
  loop {
    if stop.try_recv().is_ok() {
      report(&endpoint);
      return;
    }
    let served = slates_rt::futures::within(
      POLL_NS * 10,
      endpoint.serve_once(|_, request| {
        holder
          .with(|cell| {
            let mut holder = cell.borrow_mut();
            let Holder { held, room } = &mut *holder;
            held
              .serve(
                &mut room.space(),
                host,
                &request,
                |_, _| true,
                |_, _, _| true,
              )
              .0
          })
          .unwrap_or_default()
      }),
    )
    .await;
    if matches!(served, Ok(Some(Err(_)))) {
      report(&endpoint);
      return;
    }
  }
}

/// What a holder's sending end counted once its serve loop stopped: the losses it declared that the reader then
/// acknowledged (reordering read as loss, `slates_transport::reorder`), and its congestion window at the end.
struct SenderStats {
  spurious_losses: u64,
  congestion_window: u64,
}

/// What one reader's timed fetch measured.
struct ReaderReport {
  completion_ns: Option<u64>,
  hedges: u64,
  steals: u64,
  failed_holders: u64,
  rebuilt: bool,
}

/// One fetch of the archive over `sessions` at `hedge_after_ns`: what it measured, its chunks' latencies, and the
/// sessions back (ordered by host).
async fn fetch_once(
  sessions: Vec<(HostId, Endpoint)>,
  hedge_after_ns: u64,
) -> (ReaderReport, Vec<u64>, Vec<(HostId, Endpoint)>) {
  let original = archive();
  let (object, identity) = (ObjectId::new(OWNER, 1), original.manifest_identity());
  let reader = RefCell::new((ContentHold::new(), Room::new()));
  let timing = FetchTiming {
    deadline_ns: DEADLINE_NS,
    hedge_after_ns,
    poll_ns: POLL_NS,
  };
  let started = slates_rt::futures::now_ns();
  let fetched = fetch(
    sessions,
    (object, identity),
    timing,
    |manifest| {
      let (hold, room) = &mut *reader.borrow_mut();
      hold
        .stage_fetched(&mut room.space(), object, Placed::default(), manifest)
        .ok()
        .map(Option::unwrap_or_default)
    },
    |chunk| {
      let (hold, room) = &mut *reader.borrow_mut();
      hold
        .stage_piece(&mut room.space(), object, &identity, chunk)
        .is_ok()
    },
  )
  .await;
  let elapsed = slates_rt::futures::now_ns().saturating_sub(started);
  let rebuilt = fetched.complete && {
    let (hold, room) = &mut *reader.borrow_mut();
    hold.complete_stage(&mut room.space(), object).is_ok()
      && hold.archive_of(&room.arena, object, &identity) == Some(original)
  };
  let report = ReaderReport {
    completion_ns: fetched.complete.then_some(elapsed),
    hedges: fetched.hedges,
    steals: fetched.steals,
    failed_holders: fetched.failed_holders,
    rebuilt,
  };
  let mut sessions = fetched.sessions;
  sessions.sort_by_key(|(host, _)| host.0);
  (report, fetched.latencies_ns, sessions)
}

/// One reader: a warm-up fetch (its chunks' latencies set the hedge, as the daemon's fetch class learns its p95),
/// reported on `warm`; then, once `go` arrives, the timed fetch, reported on `done`.
async fn read(
  sessions: Vec<(HostId, Endpoint)>,
  (warm, go): (Sender<()>, Receiver<()>),
  done: Sender<ReaderReport>,
) {
  let (_, readings, sessions) = fetch_once(sessions, COLD_HEDGE_NS).await;
  let _ = warm.send(());
  let ((), _) = receive(go).await;
  let (report, _, _sessions) = fetch_once(sessions, p95(&readings).unwrap_or(COLD_HEDGE_NS)).await;
  let _ = done.send(report);
}

/// Runs one scenario to completion on a fresh simulation.
fn run(scenario: &Scenario) -> Outcome {
  let mut simulation = SimRuntime::new(&config(), 1).unwrap();
  let shard = simulation.shard_ids()[0];
  let (result_tx, result_rx) = channel();
  let scenario_owned = scenario.clone();
  simulation
    .spawn_on(shard, async move {
      let outcome = coordinate(scenario_owned).await;
      let _ = result_tx.send(outcome);
    })
    .unwrap();
  simulation.run_until_idle();
  result_rx.try_recv().expect("the scenario finished")
}

/// Every holder's hold of the archive, kept on the shard.
fn keep_holders(count: usize) -> Vec<Kept<RefCell<Holder>>> {
  (0..count)
    .map(|_| {
      let mut held = ContentHold::new();
      let mut room = Room::new();
      held
        .hold(
          &mut room.space(),
          ObjectId::new(OWNER, 1),
          Placed::default(),
          archive(),
        )
        .unwrap();
      slates_rt::registry::with_current(|context| context.keep(RefCell::new(Holder { held, room })))
        .unwrap()
        .unwrap()
    })
    .collect()
}

/// One (holder, reader) session's two ends before their handshake, and the ports each side's paths are set on.
struct Pair {
  holder: usize,
  reader: usize,
  server: Endpoint,
  client: Endpoint,
  ports: (u16, u16),
}

/// Every (holder, reader) pair: a socket each side, the holder's egress through its uplink, the requests on the bare
/// path.
fn pair_up(scenario: &Scenario) -> Vec<Pair> {
  let one_way = scenario.rtt_ns / 2;
  let holder_identities: Vec<Identity> = (0..scenario.holders).map(|_| identity()).collect();
  let reader_identities: Vec<Identity> = (0..scenario.readers).map(|_| identity()).collect();
  let shape = ConnectionShape::for_frame_cap(MAX_PACKET_PAYLOAD, scenario.receive_ceiling());
  let links: Vec<_> = (0..scenario.holders)
    .map(|holder| {
      let rate = scenario.holder_rate(holder);
      sim_udp_add_link(SimLink {
        rate_bits_per_second: rate,
        queue_bytes: (rate / BITS_PER_BYTE * scenario.rtt_ns / NS_PER_SECOND).max(MIN_QUEUE_BYTES),
      })
    })
    .collect();
  let mut pairs = Vec::new();
  for (holder, holder_identity) in holder_identities.iter().enumerate() {
    for (reader, reader_identity) in reader_identities.iter().enumerate() {
      let holder_socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let reader_socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let (holder_address, reader_address) = (
        holder_socket.local_addr().unwrap(),
        reader_socket.local_addr().unwrap(),
      );
      let path = if scenario.jitter_ns == 0 || scenario.jitter_in_order {
        SimPath::in_order(one_way, scenario.jitter_ns)
      } else {
        SimPath::reordering(one_way, scenario.jitter_ns)
      }
      .with_loss(scenario.loss)
      .with_mtu(MIN_DATAGRAM_BYTES);
      sim_udp_set_pair_path(
        holder_address.port(),
        reader_address.port(),
        path.through(links[holder]),
      );
      sim_udp_set_pair_path(reader_address.port(), holder_address.port(), path);
      let server = Endpoint::server(
        holder_socket,
        reader_address,
        holder_identity,
        &[reader_identity.certificate()],
        shape,
      )
      .unwrap();
      let client = Endpoint::client(
        reader_socket,
        holder_address,
        reader_identity,
        &holder_identity.certificate(),
        NAME,
        shape,
      )
      .unwrap();
      pairs.push(Pair {
        holder,
        reader,
        server,
        client,
        ports: (holder_address.port(), reader_address.port()),
      });
    }
  }
  pairs
}

/// Runs every pair's handshake at once and starts each holder end serving; each reader's sessions, the serve loops'
/// stops, and the silent holder's ports.
async fn establish(
  pairs: Vec<Pair>,
  holders: &[Kept<RefCell<Holder>>],
  scenario: &Scenario,
) -> (
  Vec<Vec<(HostId, Endpoint)>>,
  Vec<Sender<()>>,
  Vec<(u16, u16)>,
  Vec<Receiver<SenderStats>>,
) {
  let (established_tx, established_rx) = channel();
  let mut stats_receivers = Vec::new();
  let (mut stops, mut silent_ports) = (Vec::new(), Vec::new());
  let count = pairs.len();
  for pair in pairs {
    let Pair {
      holder,
      reader,
      mut server,
      mut client,
      ports,
    } = pair;
    if scenario.silent == Some(holder) {
      silent_ports.push(ports);
    }
    let (stop_tx, stop_rx) = channel();
    stops.push(stop_tx);
    let (stats_tx, stats_rx) = channel();
    stats_receivers.push(stats_rx);
    let (host, kept) = (HostId(10 + holder as u64), holders[holder]);
    let established = established_tx.clone();
    let (ready_tx, ready_rx) = channel();
    let server_task = slates_rt::futures::spawn(async move {
      server.establish().await.unwrap();
      let _ = ready_tx.send(());
      serve(server, kept, host, stop_rx, stats_tx).await;
    })
    .unwrap();
    let _ = slates_rt::futures::detach(server_task);
    let client_task = slates_rt::futures::spawn(async move {
      client.establish().await.unwrap();
      let ((), _) = receive(ready_rx).await;
      let _ = established.send((reader, host, client));
    })
    .unwrap();
    let _ = slates_rt::futures::detach(client_task);
  }
  drop(established_tx);
  let mut sessions: Vec<Vec<(HostId, Endpoint)>> =
    (0..scenario.readers).map(|_| Vec::new()).collect();
  let mut established_rx = established_rx;
  for _ in 0..count {
    let ((reader, host, client), rest) = receive(established_rx).await;
    established_rx = rest;
    sessions[reader].push((host, client));
  }
  (sessions, stops, silent_ports, stats_receivers)
}

/// Runs every reader's warm-up, then their timed fetches together, and gathers what they measured.
async fn run_readers(reader_sessions: Vec<Vec<(HostId, Endpoint)>>) -> Outcome {
  let readers = reader_sessions.len();
  let (warm_tx, warm_rx) = channel();
  let (done_tx, done_rx) = channel();
  let mut gos = Vec::new();
  for mut sessions in reader_sessions {
    sessions.sort_by_key(|(host, _)| host.0);
    let (go_tx, go_rx) = channel();
    gos.push(go_tx);
    let task =
      slates_rt::futures::spawn(read(sessions, (warm_tx.clone(), go_rx), done_tx.clone())).unwrap();
    let _ = slates_rt::futures::detach(task);
  }
  drop((warm_tx, done_tx));
  // The timed fetches start together, once every warm-up is done.
  let mut warm_rx = warm_rx;
  for _ in 0..readers {
    let ((), rest) = receive(warm_rx).await;
    warm_rx = rest;
  }
  for go in &gos {
    let _ = go.send(());
  }
  let mut outcome = Outcome {
    completions_ns: Vec::new(),
    hedges: 0,
    steals: 0,
    failed_holders: 0,
    all_rebuilt: true,
    spurious_losses: 0,
    min_congestion_window: u64::MAX,
  };
  let mut done_rx = done_rx;
  for _ in 0..readers {
    let (report, rest) = receive(done_rx).await;
    done_rx = rest;
    match report.completion_ns {
      Some(ns) => outcome.completions_ns.push(ns),
      None => outcome.all_rebuilt = false,
    }
    outcome.hedges += report.hedges;
    outcome.steals += report.steals;
    outcome.failed_holders += report.failed_holders;
    outcome.all_rebuilt &= report.rebuilt;
  }
  outcome
}

/// Builds the network and every role, makes the silent holder silent once its sessions formed, runs the readers,
/// and stops the holders.
async fn coordinate(scenario: Scenario) -> Outcome {
  let holders = keep_holders(scenario.holders);
  let pairs = pair_up(&scenario);
  let (reader_sessions, stops, silent_ports, stats) = establish(pairs, &holders, &scenario).await;
  // A silent holder: every one of its paths drops everything from here on.
  for (holder_port, reader_port) in silent_ports {
    let dead = SimPath::in_order(scenario.rtt_ns / 2, 0).with_loss(SimLoss::random(PPM));
    sim_udp_set_pair_path(holder_port, reader_port, dead);
    sim_udp_set_pair_path(reader_port, holder_port, dead);
  }
  let mut outcome = run_readers(reader_sessions).await;
  for stop in stops {
    let _ = stop.send(());
  }
  for receiver in stats {
    let (sender, _) = receive(receiver).await;
    outcome.spurious_losses += sender.spurious_losses;
    outcome.min_congestion_window = outcome.min_congestion_window.min(sender.congestion_window);
  }
  outcome
}

fn grid() -> Vec<Scenario> {
  let base = |name, holders, readers, rtt_ms: u64, mbps: u64, loss| Scenario {
    name,
    holders,
    readers,
    rtt_ns: rtt_ms * NS_PER_MS,
    rate_bits_per_second: mbps * 1_000_000,
    loss,
    jitter_ns: 0,
    jitter_in_order: false,
    slow: None,
    silent: None,
  };
  let lan_rtt_ms = 1;
  let wan_rtt_ms = 80;
  vec![
    base("lan 1 holder", 1, 1, lan_rtt_ms, 1000, SimLoss::NONE),
    base("lan 3 holders", 3, 1, lan_rtt_ms, 1000, SimLoss::NONE),
    base("wan 1 holder", 1, 1, wan_rtt_ms, 100, SimLoss::NONE),
    base("wan 3 holders", 3, 1, wan_rtt_ms, 100, SimLoss::NONE),
    base(
      "wan 1 holder 1% loss",
      1,
      1,
      wan_rtt_ms,
      100,
      SimLoss::random(PPM / 100),
    ),
    base(
      "wan 3 holders 1% loss",
      3,
      1,
      wan_rtt_ms,
      100,
      SimLoss::random(PPM / 100),
    ),
    base(
      "wan 3 holders 5% loss",
      3,
      1,
      wan_rtt_ms,
      100,
      SimLoss::random(PPM / 20),
    ),
    Scenario {
      slow: Some((0, 20)),
      ..base(
        "wan 3 holders, one at 1/20 rate",
        3,
        1,
        wan_rtt_ms,
        100,
        SimLoss::NONE,
      )
    },
    Scenario {
      silent: Some(0),
      ..base(
        "wan 3 holders, one silent",
        3,
        1,
        wan_rtt_ms,
        100,
        SimLoss::NONE,
      )
    },
    base(
      "wan 3 holders, 8 readers",
      3,
      8,
      wan_rtt_ms,
      100,
      SimLoss::NONE,
    ),
    base(
      "wan 3 holders, 8 readers, 1% loss",
      3,
      8,
      wan_rtt_ms,
      100,
      SimLoss::random(PPM / 100),
    ),
    // The horrific profiles (condition 7: "heavy contested usage, network congestion, network issues"): an
    // intercontinental path with reordering jitter and heavy loss, a congested many-reader pull at 10% loss, and a
    // thin lossy link that reorders.
    Scenario {
      jitter_ns: 60 * NS_PER_MS,
      ..base(
        "horrific: 250 ms, reordering ±60 ms, 5% loss, 3 holders",
        3,
        1,
        250,
        100,
        SimLoss::random(PPM / 20),
      )
    },
    base(
      "horrific: 3 holders, 8 readers, 10% loss",
      3,
      8,
      wan_rtt_ms,
      100,
      SimLoss::random(PPM / 10),
    ),
    // The thin link's two impairments apart, so a slow pull names which one costs it.
    Scenario {
      jitter_ns: 40 * NS_PER_MS,
      ..base(
        "horrific parts: thin 10 Mbit/s, 200 ms, reordering ±40 ms, no loss",
        1,
        1,
        200,
        10,
        SimLoss::NONE,
      )
    },
    Scenario {
      jitter_ns: 40 * NS_PER_MS,
      jitter_in_order: true,
      ..base(
        "horrific parts: thin 10 Mbit/s, 200 ms, jitter ±40 ms in order, no loss",
        1,
        1,
        200,
        10,
        SimLoss::NONE,
      )
    },
    base(
      "horrific parts: thin 10 Mbit/s, 200 ms, 2% loss, in order",
      1,
      1,
      200,
      10,
      SimLoss::random(PPM / 50),
    ),
    Scenario {
      jitter_ns: 40 * NS_PER_MS,
      ..base(
        "horrific: thin 10 Mbit/s, 200 ms, reordering ±40 ms, 2% loss",
        1,
        1,
        200,
        10,
        SimLoss::random(PPM / 50),
      )
    },
  ]
}

fn main() {
  let filter = std::env::args().nth(1).unwrap_or_default();
  let archive_bytes = (chunks() * CHUNK_BYTES) as u64;
  println!(
    "scenario,holders,readers,rtt_ms,uplink_mbps,completion_min_ms,completion_median_ms,completion_max_ms,goodput_mbps,capacity_mbps,hedges,steals,holders_dropped,rebuilt,spurious_losses,min_end_cwnd_bytes"
  );
  let mut failed = false;
  for scenario in grid()
    .into_iter()
    .filter(|scenario| scenario.name.contains(&filter))
  {
    let outcome = run(&scenario);
    let mut completions = outcome.completions_ns.clone();
    completions.sort_unstable();
    let ms = |ns: u64| ns as f64 / NS_PER_MS as f64;
    let (min, median, max) = (
      completions.first().copied().unwrap_or(0),
      completions.get(completions.len() / 2).copied().unwrap_or(0),
      completions.last().copied().unwrap_or(0),
    );
    let goodput = if max > 0 {
      (archive_bytes * scenario.readers as u64 * BITS_PER_BYTE) as f64
        / (max as f64 / NS_PER_SECOND as f64)
        / 1e6
    } else {
      0.0
    };
    let capacity: u64 = (0..scenario.holders)
      .map(|holder| scenario.holder_rate(holder))
      .sum::<u64>()
      / 1_000_000;
    println!(
      "{},{},{},{},{},{:.1},{:.1},{:.1},{:.1},{},{},{},{},{},{},{}",
      scenario.name,
      scenario.holders,
      scenario.readers,
      scenario.rtt_ns / NS_PER_MS,
      scenario.rate_bits_per_second / 1_000_000,
      ms(min),
      ms(median),
      ms(max),
      goodput,
      capacity,
      outcome.hedges,
      outcome.steals,
      outcome.failed_holders,
      outcome.all_rebuilt,
      outcome.spurious_losses,
      outcome.min_congestion_window
    );
    failed |= !outcome.all_rebuilt;
  }
  if failed {
    std::process::exit(1);
  }
}
