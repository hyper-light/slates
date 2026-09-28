//! The session plane's congestion benchmark (§4.10a; the constrained-link design,
//! `docs/wip/research/nfs-transport-constrained-links.md` §5.3 and §9): real endpoints — TLS 1.3
//! handshake, packet protection, the clocked connection with its Copa controller — over the simulated
//! network (`crates/rt/src/sim.rs`), across the scenarios the research note names.
//!
//! This grid decided the controller (the 2026-09-28 bake-off, `docs/wip/BENCHMARKS.md`: Copa over NewReno,
//! CUBIC, BBRv3 and Copa-Meta, the losers deleted); it now records the chosen controller on the same grid,
//! so a regression shows against the recorded numbers.
//!
//! One run: a bulk flow (a client sends `bulk_bytes` on one stream) and a ping flow (a request/reply of
//! [`PING_BYTES`] at a gap holding its load to [`PING_LOAD_PERMILLE`] of the link) share one bottleneck
//! toward the servers; the reverse direction is the propagation delay alone. The ping flow's request
//! latency is the headline metric — how long a small request waits behind bulk data at the bottleneck
//! (research note §5.3) — beside the bulk goodput against the link's capacity. Two-flow scenarios report
//! Jain's fairness index of the flows' goodputs.
//!
//! **Failures** (the process exits non-zero): any run that stalls past its bound
//! ([`Scenario::stall_bound_ns`]), or a two-flow scenario scoring Jain below [`FAIRNESS_FLOOR`].
//!
//! `cargo run --release -p slates-transport --example congestion_bench [scenario-filter]` prints one CSV row
//! per run, then the per-scenario summary. Deterministic from the seeds (the simulation's virtual clock).
//! `BAKEOFF_PROGRESS=1` reports virtual progress; `BAKEOFF_TRACE=1` prints every ping.

// Benchmark harness: an unwrap here is a failed run, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::cast_precision_loss,
  clippy::cast_possible_truncation,
  clippy::cast_sign_loss
)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};

use rustls::pki_types::PrivateKeyDer;
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::{
  PPM, SimLink, SimLoss, SimPath, SimRuntime, sim_udp_add_link, sim_udp_set_link,
  sim_udp_set_pair_path, sim_udp_set_path, sim_udp_stats,
};
use slates_rt::udp::{Ipv4Addr, SocketAddrV4, UdpSocket};
use slates_transport::connection::{ConnectionShape, Priority, initial_receive_window};
use slates_transport::endpoint::{Endpoint, MAX_PACKET_PAYLOAD, MIN_DATAGRAM_BYTES};
use slates_transport::handshake::Identity;
use slates_transport::session::STREAM_FRAME_HEADER_BYTES;

/// Derived: the wire bytes each packet adds around a frame's data — the short header and AEAD tag
/// (`MIN_DATAGRAM_BYTES − MAX_PACKET_PAYLOAD`) and one `Stream` frame header — for sizing a class's load.
const WIRE_OVERHEAD: usize = MIN_DATAGRAM_BYTES - MAX_PACKET_PAYLOAD + STREAM_FRAME_HEADER_BYTES;
/// Format: the fleet's packet budget, so the benchmark frames exactly as the daemon does.
const FRAME_CAP: usize = MAX_PACKET_PAYLOAD;
/// Shape: the ping request and reply size — a small metadata operation.
const PING_BYTES: usize = 64;
/// Shape: the steady-state pings each run collects — enough that the p99 is a percentile (the tenth-worst
/// sample), not the second-worst of a hundred and fifty, which a single loss recovery decides (the first
/// full grid, 2026-09-27, measured about 150 per run and could not resolve the tail).
const PINGS: u64 = 1000;
/// Shape: the ping flow's share of the link, per mille — light enough that it measures the bulk flow's
/// queue rather than adding its own.
const PING_LOAD_PERMILLE: u64 = 10;
/// Shape: the warm-up before the steady window, in round trips (and at least [`MIN_WARMUP_NS`]) — past
/// the controller's startup.
const WARMUP_RTTS: u64 = 20;
/// Shape: the warm-up's floor, 5 s.
const MIN_WARMUP_NS: u64 = 5_000_000_000;
/// Shape: the server name the identities carry.
const NAME: &str = "slates-bench";
/// Format: nanoseconds per millisecond.
const MS: u64 = 1_000_000;
/// Shape: the receive ceiling in BDPs — generous, so the controller, never flow control, is the limit.
const CEILING_BDPS: u64 = 8;

/// Shape: every simulated host's interface MTU — Ethernet (1,500 bytes). The path's own MTU stays the floor.
const ETHERNET_INTERFACE_MTU: usize = 1_500;
/// Shape: the seeds each scenario runs with.
const SEEDS: [u64; 3] = [1, 2, 3];
/// Shape: a run is recorded as stalled once it has taken this many times its transfer's ideal duration
/// at link rate — it delivered under 1 % of the link's capacity. Far past the worst healthy result in
/// the first grid (NewReno at 4 % of capacity, about 25× ideal, 2026-09-27), so only a genuine stall
/// trips it; before this bound one Copa run spun for 2.5 h of wall time and held up the whole grid.
const STALL_FACTOR: u64 = 100;
/// Shape: the fairness floor (Jain's index, 1 is perfectly fair) — the bake-off's disqualification line.
const FAIRNESS_FLOOR: f64 = 0.9;

/// A bandwidth change during the run: at `at_ns`, the link becomes `rate` bits per second.
#[derive(Clone, Copy, Debug)]
struct Step {
  at_ns: u64,
  rate: u64,
}

/// One scenario of the network under test.
#[derive(Clone, Debug)]
struct Scenario {
  name: String,
  rate: u64,
  rtt_ns: u64,
  buffer_bdp_permille: u64,
  loss: SimLoss,
  jitter_ns: u64,
  reorders: bool,
  /// A second bulk flow's round trip (a fairness scenario).
  second_flow: Option<u64>,
  steps: Vec<Step>,
}

impl Scenario {
  fn bdp(&self) -> u64 {
    (u128::from(self.rate) * u128::from(self.rtt_ns) / (8 * 1_000_000_000)) as u64
  }

  /// The gap between pings that holds their load (request and reply packets, each the ping plus the
  /// packet overhead) to [`PING_LOAD_PERMILLE`] of the link.
  fn ping_gap_ns(&self) -> u64 {
    let bits_per_ping = 2 * (PING_BYTES + WIRE_OVERHEAD) as u64 * 8;
    bits_per_ping * 1_000_000_000 * 1000 / (self.rate.max(1) * PING_LOAD_PERMILLE)
  }

  /// The warm-up before the steady window.
  fn warmup_ns(&self) -> u64 {
    (WARMUP_RTTS * self.rtt_ns).max(MIN_WARMUP_NS)
  }

  /// The bulk transfer: the link's capacity over the warm-up plus the time [`PINGS`] pings take, so the
  /// bulk flow loads the link for every steady ping.
  fn bulk_bytes(&self) -> u64 {
    let duration = self.warmup_ns() + PINGS * self.ping_gap_ns();
    (u128::from(self.rate / 8) * u128::from(duration) / 1_000_000_000) as u64
  }

  /// The virtual time past which a run is recorded as stalled: [`STALL_FACTOR`] times the transfer's
  /// ideal duration at the link's rate, plus the warm-up.
  fn stall_bound_ns(&self) -> u64 {
    let ideal_ns = u128::from(self.bulk_bytes()) * 8 * 1_000_000_000 / u128::from(self.rate.max(1));
    u64::try_from(ideal_ns.saturating_mul(u128::from(STALL_FACTOR)))
      .unwrap_or(u64::MAX)
      .saturating_add(self.warmup_ns())
  }

  fn queue_bytes(&self) -> u64 {
    (self.bdp().saturating_mul(self.buffer_bdp_permille) / 1000).max(2 * MIN_DATAGRAM_BYTES as u64)
  }
}

/// What one run measured.
#[derive(Clone, Debug)]
struct Outcome {
  goodputs: Vec<f64>,
  /// Each ping: when it was sent (from the start of the run) and its latency.
  pings_ns: Vec<(u64, u64)>,
  /// When steady state begins: past the first ten round trips and the first quarter of the transfer.
  steady_from: u64,
  peak_queue: u64,
  dropped_queue: u64,
  dropped_loss: u64,
  stalled: bool,
}

impl Outcome {
  fn percentile(&self, permille: usize) -> f64 {
    percentile(
      self.pings_ns.iter().map(|(_, latency)| *latency).collect(),
      permille,
    )
  }

  /// The percentile over the steady-state pings only.
  fn steady_percentile(&self, permille: usize) -> f64 {
    percentile(
      self
        .pings_ns
        .iter()
        .filter(|(at, _)| *at >= self.steady_from)
        .map(|(_, latency)| *latency)
        .collect(),
      permille,
    )
  }

  fn jain(&self) -> f64 {
    let sum: f64 = self.goodputs.iter().sum();
    let squares: f64 = self.goodputs.iter().map(|g| g * g).sum();
    if squares == 0.0 {
      return 0.0;
    }
    sum * sum / (self.goodputs.len() as f64 * squares)
  }
}

/// The `permille` percentile of `values` (nanoseconds) in milliseconds; infinite when empty.
fn percentile(mut values: Vec<u64>, permille: usize) -> f64 {
  if values.is_empty() {
    return f64::INFINITY;
  }
  values.sort_unstable();
  let index = (values.len() * permille / 1000).min(values.len() - 1);
  values[index] as f64 / MS as f64
}

fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 1,
    tasks_per_shard: 64,
    timers_per_shard: 256,
    ring_entries: 64,
    step_budget_ns: 1_000_000_000,
    timer_tick_ns: 10_000,
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

fn shape(scenario: &Scenario) -> ConnectionShape {
  let ceiling = scenario
    .bdp()
    .saturating_mul(CEILING_BDPS)
    .max(64 * initial_receive_window(FRAME_CAP));
  ConnectionShape::for_frame_cap(FRAME_CAP, ceiling)
}

static BULK_DONE: AtomicBool = AtomicBool::new(false);

/// A client/server pair's sockets bound and their ports known.
struct Pair {
  client: UdpSocket,
  server: UdpSocket,
}

impl Pair {
  fn bind() -> Pair {
    let any = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
    Pair {
      client: UdpSocket::bind(any).unwrap(),
      server: UdpSocket::bind(any).unwrap(),
    }
  }

  fn ports(&self) -> (u16, u16) {
    (
      self.client.local_addr().unwrap().port(),
      self.server.local_addr().unwrap().port(),
    )
  }
}

/// Runs one scenario with one seed.
fn run(scenario: &Scenario, seed: u64) -> Outcome {
  BULK_DONE.store(false, Ordering::Release);
  let mut sim = SimRuntime::new(&config(), seed).unwrap();
  let shard = sim.shard_ids()[0];
  let (done_tx, done_rx): (Sender<(usize, u64, u64)>, _) = channel();
  let (ping_tx, ping_rx): (Sender<(u64, u64)>, _) = channel();
  let scenario_task = scenario.clone();
  sim
    .spawn_on(shard, async move {
      coordinator(scenario_task, done_tx, ping_tx).await;
    })
    .unwrap();
  let wall = std::time::Instant::now();
  let steps_run = sim.run_until_idle();
  if std::env::var_os("BAKEOFF_PROGRESS").is_some() {
    eprintln!(
      "steps {steps_run} virtual {:.1}s wall {:.2}s",
      sim.now_ns() as f64 / 1e9,
      wall.elapsed().as_secs_f64()
    );
  }
  let flows = 1 + usize::from(scenario.second_flow.is_some());
  let finished: Vec<(usize, u64, u64)> = done_rx.try_iter().collect();
  let stats = sim_udp_stats();
  let stalled = finished.len() != flows || steps_run == 0;
  let mut goodputs = vec![0.0; flows];
  for (flow, bytes, elapsed) in &finished {
    goodputs[*flow] = *bytes as f64 * 8.0 / (*elapsed as f64 / 1e9);
  }
  Outcome {
    goodputs,
    pings_ns: ping_rx.try_iter().collect(),
    steady_from: scenario.warmup_ns(),
    peak_queue: stats.peak_queue_bytes,
    dropped_queue: stats.dropped_queue,
    dropped_loss: stats.dropped_loss,
    stalled,
  }
}

/// Builds the network, starts every role as a child task, waits for the bulk flows, then ends the rest.
async fn coordinator(
  scenario: Scenario,
  done: Sender<(usize, u64, u64)>,
  pings: Sender<(u64, u64)>,
) {
  let one_way = scenario.rtt_ns / 2;
  // Every host is Ethernet behind the floor path (a tunnel, a VPN): its interface refuses a datagram past
  // 1,500 bytes at the send, as a real host with don't-fragment set does, so path MTU discovery pays only for
  // the sizes the interface allows (RFC 8899 §4.4) — the floor path then bounds what crosses.
  slates_rt::sim::sim_udp_set_interface_mtu(Some(ETHERNET_INTERFACE_MTU));
  let reverse = SimPath::in_order(one_way, 0)
    .with_loss(scenario.loss)
    .with_mtu(MIN_DATAGRAM_BYTES);
  sim_udp_set_path(reverse);
  let link = sim_udp_add_link(SimLink {
    rate_bits_per_second: scenario.rate,
    queue_bytes: scenario.queue_bytes(),
  });
  let forward_for = |rtt: u64| {
    let base = if scenario.reorders {
      SimPath::reordering(rtt / 2, scenario.jitter_ns)
    } else {
      SimPath::in_order(rtt / 2, scenario.jitter_ns)
    };
    base
      .with_loss(scenario.loss)
      .with_mtu(MIN_DATAGRAM_BYTES)
      .through(link)
  };
  let ping_pair = Pair::bind();
  let (ping_client, ping_server) = ping_pair.ports();
  sim_udp_set_pair_path(ping_client, ping_server, forward_for(scenario.rtt_ns));
  let mut flows = vec![(Pair::bind(), scenario.rtt_ns)];
  if let Some(rtt) = scenario.second_flow {
    flows.push((Pair::bind(), rtt));
  }
  for (pair, rtt) in &flows {
    let (client, server) = pair.ports();
    sim_udp_set_pair_path(client, server, forward_for(*rtt));
    if *rtt != scenario.rtt_ns {
      sim_udp_set_pair_path(
        server,
        client,
        SimPath::in_order(rtt / 2, 0)
          .with_loss(scenario.loss)
          .with_mtu(MIN_DATAGRAM_BYTES),
      );
    }
  }
  let bulk = vec![0xB5u8; usize::try_from(scenario.bulk_bytes()).unwrap()];
  let mut servers = Vec::new();
  let mut clients = Vec::new();
  let (bulk_done_tx, bulk_done_rx) = channel::<()>();
  for (index, (pair, _)) in flows.into_iter().enumerate() {
    let flow_shape = shape(&scenario);
    let client_identity = self_signed(NAME);
    let server_identity = self_signed(NAME);
    let (client_port, server_port) = pair.ports();
    let server_cert = server_identity.certificate();
    let client_cert = client_identity.certificate();
    let Pair { client, server } = pair;
    let expected = bulk.len();
    let done = done.clone();
    let bulk_done_tx = bulk_done_tx.clone();
    servers.push(
      slates_rt::futures::spawn_child(async move {
        let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, client_port);
        let mut endpoint =
          Endpoint::server(server, peer, &server_identity, &[client_cert], flow_shape).unwrap();
        if let Err(e) = endpoint.establish().await {
          eprintln!("ROLE bulk server {index} establish: {e:?}");
          return;
        }
        let started = slates_rt::futures::now_ns();
        let (id, received) = match endpoint.next_request().await {
          Ok((id, _kind, received)) => (id, received),
          Err(e) => {
            eprintln!("ROLE bulk server {index} recv: {e:?}");
            return;
          }
        };
        let elapsed = slates_rt::futures::now_ns() - started;
        assert_eq!(received.len(), expected, "the bulk stream arrived whole");
        let _ = done.send((index, received.len() as u64, elapsed));
        let _ = bulk_done_tx.send(());
        // Acknowledge the transfer and keep the session driven until the coordinator ends the run.
        if let Err(e) = endpoint.reply(id, &[]) {
          eprintln!("ROLE bulk server {index} reply: {e:?}");
          return;
        }
        let _ = endpoint.settle().await;
      })
      .unwrap(),
    );
    let data = bulk.clone();
    clients.push(
      slates_rt::futures::spawn_child(async move {
        let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
        let mut endpoint = Endpoint::client(
          client,
          peer,
          &client_identity,
          &server_cert,
          NAME,
          flow_shape,
        )
        .unwrap();
        endpoint.establish().await.unwrap();
        if let Err(e) = endpoint.request(1, Priority::Bulk, &data).await {
          eprintln!("ROLE bulk client: {e:?}");
        }
      })
      .unwrap(),
    );
  }
  let ping_shape = shape(&scenario);
  let ping_server_identity = self_signed(NAME);
  let ping_client_identity = self_signed(NAME);
  let ping_server_cert = ping_server_identity.certificate();
  let ping_client_cert = ping_client_identity.certificate();
  let Pair { client, server } = ping_pair;
  let ping_server_task = slates_rt::futures::spawn_child(async move {
    let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, ping_client);
    let mut endpoint = Endpoint::server(
      server,
      peer,
      &ping_server_identity,
      &[ping_client_cert],
      ping_shape,
    )
    .unwrap();
    endpoint.establish().await.unwrap();
    loop {
      endpoint.serve_once(|_, request| request).await.unwrap();
    }
  })
  .unwrap();
  let run_start = slates_rt::futures::now_ns();
  let ping_gap = scenario.ping_gap_ns();
  let ping_client_task = slates_rt::futures::spawn_child(async move {
    let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, ping_server);
    let mut endpoint = Endpoint::client(
      client,
      peer,
      &ping_client_identity,
      &ping_server_cert,
      NAME,
      ping_shape,
    )
    .unwrap();
    endpoint.establish().await.unwrap();
    while !BULK_DONE.load(Ordering::Acquire) {
      let started = slates_rt::futures::now_ns();
      let reply = endpoint
        .request(1, Priority::Control, &[0x50u8; PING_BYTES])
        .await
        .unwrap();
      assert_eq!(reply.len(), PING_BYTES, "the ping echoed");
      let _ = pings.send((started - run_start, slates_rt::futures::now_ns() - started));
      slates_rt::futures::sleep(ping_gap).await;
    }
  })
  .unwrap();
  let flows = 1 + usize::from(scenario.second_flow.is_some());
  let completed = wait_for_bulk(&scenario, link, flows, bulk_done_rx).await;
  BULK_DONE.store(true, Ordering::Release);
  if completed {
    let _ = slates_rt::futures::join(ping_client_task).await;
  } else {
    // A stalled run: every role may be mid-exchange forever, so each is cancelled, not joined.
    let _ = slates_rt::futures::cancel(ping_client_task);
    for client in clients {
      let _ = slates_rt::futures::cancel(client);
    }
  }
  let _ = slates_rt::futures::cancel(ping_server_task);
  for server in servers {
    let _ = slates_rt::futures::cancel(server);
  }
}

/// Waits until `flows` bulk flows have reported done, applying the scenario's bandwidth steps to `link` at
/// their times meanwhile; `false` when the run passed its stall bound first ([`Scenario::stall_bound_ns`]).
async fn wait_for_bulk(
  scenario: &Scenario,
  link: slates_rt::sim::SimLinkId,
  flows: usize,
  bulk_done_rx: std::sync::mpsc::Receiver<()>,
) -> bool {
  let start = slates_rt::futures::now_ns();
  let mut pending_steps = scenario.steps.clone();
  let mut finished = 0;
  while finished < flows {
    if slates_rt::futures::now_ns().saturating_sub(start) > scenario.stall_bound_ns() {
      eprintln!(
        "STALL {}: {finished} of {flows} flows done at the {} s bound",
        scenario.name,
        scenario.stall_bound_ns() / (1000 * MS)
      );
      return false;
    }
    if let Ok(()) = bulk_done_rx.try_recv() {
      finished += 1;
      continue;
    }
    let now = slates_rt::futures::now_ns() - start;
    if std::env::var_os("BAKEOFF_PROGRESS").is_some() && now % (10_000 * MS) < MS {
      eprintln!(
        "progress: {} s virtual, {finished} of {flows} flows done",
        now / (1000 * MS)
      );
    }
    pending_steps.retain(|step| {
      if step.at_ns > now {
        return true;
      }
      sim_udp_set_link(
        link,
        SimLink {
          rate_bits_per_second: step.rate,
          queue_bytes: scenario.queue_bytes(),
        },
      );
      false
    });
    slates_rt::futures::sleep(MS).await;
  }
  true
}

fn scenarios() -> Vec<Scenario> {
  let base = |name: String, rate: u64, rtt_ms: u64, loss: SimLoss| Scenario {
    name,
    rate,
    rtt_ns: rtt_ms * MS,
    buffer_bdp_permille: 1000,
    loss,
    jitter_ns: 0,
    reorders: false,
    second_flow: None,
    steps: Vec::new(),
  };
  let mut all = Vec::new();
  // The grid (research note §9): rate × RTT × random loss, one-BDP buffer.
  for rate in [64_000, 1_000_000, 10_000_000, 100_000_000] {
    for rtt in [20, 100, 300] {
      for (loss_label, loss) in [
        ("0", SimLoss::NONE),
        ("0.1%", SimLoss::random(PPM / 1000)),
        ("1%", SimLoss::random(PPM / 100)),
        ("5%", SimLoss::random(PPM / 20)),
      ] {
        all.push(base(
          format!("grid rate={rate} rtt={rtt}ms loss={loss_label}"),
          rate,
          rtt,
          loss,
        ));
      }
    }
  }
  // Buffer depth: shallow, one BDP, bufferbloat — at 10 Mbit/s, 100 ms.
  for (label, permille) in [("0.25", 250), ("4", 4000), ("16", 16_000)] {
    let mut scenario = base(
      format!("buffer={label}xBDP rate=10M rtt=100ms"),
      10_000_000,
      100,
      SimLoss::NONE,
    );
    scenario.buffer_bdp_permille = permille;
    all.push(scenario);
  }
  // Reordering: ±8 ms jitter on a 20 ms path.
  let mut reorder = base(
    "reorder jitter=8ms rate=10M rtt=20ms".to_owned(),
    10_000_000,
    20,
    SimLoss::NONE,
  );
  reorder.jitter_ns = 8 * MS;
  reorder.reorders = true;
  all.push(reorder);
  // Burst loss: bursts averaging four datagrams, about 1% overall.
  all.push(base(
    "burst loss rate=10M rtt=100ms".to_owned(),
    10_000_000,
    100,
    SimLoss::bursty(PPM / 400, PPM / 4, PPM),
  ));
  // A bandwidth step: 10 → 2 → 10 Mbit/s.
  let mut step = base(
    "step 10M-2M-10M rtt=100ms".to_owned(),
    10_000_000,
    100,
    SimLoss::NONE,
  );
  step.steps = vec![
    Step {
      at_ns: 2_000 * MS,
      rate: 2_000_000,
    },
    Step {
      at_ns: 6_000 * MS,
      rate: 10_000_000,
    },
  ];
  all.push(step);
  // Fairness: two flows, equal RTT and unequal RTT.
  let mut fair = base(
    "fair 2 flows rate=10M rtt=50ms".to_owned(),
    10_000_000,
    50,
    SimLoss::NONE,
  );
  fair.second_flow = Some(50 * MS);
  all.push(fair);
  let mut rtt_fair = base(
    "rtt-fair 2 flows 20ms vs 100ms rate=10M".to_owned(),
    10_000_000,
    20,
    SimLoss::NONE,
  );
  rtt_fair.second_flow = Some(100 * MS);
  all.push(rtt_fair);
  all
}

fn main() {
  let filter = std::env::args().nth(1);
  println!(
    "scenario,seed,goodput_mbps,capacity_share,ping_p50_ms,ping_p99_ms,ping_max_ms,steady_p50_ms,steady_p99_ms,pings,peak_queue_kb,dropped_queue,dropped_loss,jain,stalled"
  );
  let mut rows: Vec<(String, Outcome)> = Vec::new();
  for scenario in scenarios() {
    if filter
      .as_ref()
      .is_some_and(|f| !scenario.name.contains(f.as_str()))
    {
      continue;
    }
    for seed in SEEDS {
      let outcome = run(&scenario, seed);
      let goodput = outcome.goodputs.first().copied().unwrap_or(0.0);
      println!(
        "{},{},{:.3},{:.3},{:.1},{:.1},{:.1},{:.1},{:.1},{},{:.1},{},{},{:.3},{}",
        scenario.name,
        seed,
        goodput / 1e6,
        goodput / scenario.rate as f64,
        outcome.percentile(500),
        outcome.percentile(990),
        outcome.percentile(1000),
        outcome.steady_percentile(500),
        outcome.steady_percentile(990),
        outcome.pings_ns.len(),
        outcome.peak_queue as f64 / 1024.0,
        outcome.dropped_queue,
        outcome.dropped_loss,
        outcome.jain(),
        outcome.stalled
      );
      if std::env::var_os("BAKEOFF_TRACE").is_some() {
        for (at, latency) in &outcome.pings_ns {
          println!(
            "trace,{},{},{:.1},{:.1}",
            scenario.name,
            seed,
            *at as f64 / MS as f64,
            *latency as f64 / MS as f64
          );
        }
      }
      rows.push((scenario.name.clone(), outcome));
    }
  }
  if !report(&rows) {
    std::process::exit(1);
  }
}

/// Prints each scenario's mean steady p99 and goodput share across its seeds, and every failure (a stall,
/// or a two-flow scenario below the fairness floor); returns whether there were none.
fn report(rows: &[(String, Outcome)]) -> bool {
  let names: std::collections::BTreeSet<&String> = rows.iter().map(|(name, _)| name).collect();
  let mut failures: Vec<String> = Vec::new();
  println!();
  println!("scenario,mean_steady_p99_ms,worst_steady_p99_ms,mean_capacity_share");
  for name in names {
    let runs: Vec<&Outcome> = rows
      .iter()
      .filter(|(n, _)| n == name)
      .map(|(_, outcome)| outcome)
      .collect();
    for outcome in &runs {
      if outcome.stalled {
        failures.push(format!("stalled in {name}"));
      }
      if outcome.goodputs.len() == 2 && outcome.jain() < FAIRNESS_FLOOR {
        failures.push(format!("Jain {:.3} in {name}", outcome.jain()));
      }
    }
    let count = runs.len().max(1) as f64;
    let p99s: Vec<f64> = runs.iter().map(|o| o.steady_percentile(990)).collect();
    let mean_p99 = p99s.iter().sum::<f64>() / count;
    let worst_p99 = p99s.iter().copied().fold(0.0, f64::max);
    let rate = scenarios()
      .iter()
      .find(|scenario| &scenario.name == name)
      .map_or(1.0, |scenario| scenario.rate as f64);
    let share = runs
      .iter()
      .map(|o| o.goodputs.first().copied().unwrap_or(0.0) / rate)
      .sum::<f64>()
      / count;
    println!("{name},{mean_p99:.1},{worst_p99:.1},{share:.3}");
  }
  println!();
  if failures.is_empty() {
    println!("failures: none");
  } else {
    println!("failures: {}", failures.join("; "));
  }
  failures.is_empty()
}
