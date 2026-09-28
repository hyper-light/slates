//! The session plane's stream-scheduler bake-off (§4.10a; the constrained-link design,
//! `docs/wip/research/nfs-transport-constrained-links.md` §5.3): real endpoints — TLS 1.3 handshake,
//! packet protection, the clocked connection — over the simulated network, each scheduler in turn
//! (`connection::Scheduler`: round-robin over every stream, strict priority by class, and deficit
//! round-robin weighted by class).
//!
//! One run: a single session crosses a bottleneck toward the server carrying all three classes at once,
//! as a fleet record session does — bulk transfers ([`BULK_FLOWS`] at a time, each restarted as it
//! completes) keep the link full; metadata exchanges (multi-packet requests and replies) use
//! [`METADATA_LOAD_PERMILLE`] of the link; control pings use [`CONTROL_LOAD_PERMILLE`]. Each class's
//! latency and completions and the bulk goodput are measured over the steady window.
//!
//! The selection rule, fixed before any run (a winner chosen after peeking is not evidence):
//! 1. disqualified if any run stalls or starves a class (a class completing nothing in the window);
//! 2. primary: the control class's p99 latency, as a geometric mean over scenarios of the scheduler's p99
//!    over the best scheduler's, with each scheduler's worst scenario reported beside it — the p99 must be
//!    solid everywhere, not on average;
//! 3. then the metadata class's p99, the same way;
//! 4. then the bulk goodput shortfall, the same way.
//!
//! `cargo run --release -p slates-transport --example scheduler_bakeoff [scenario-filter]` prints one CSV
//! row per run and the ranking.
//!
//! Every simulated path carries the datagram floor as its MTU (`MIN_DATAGRAM_BYTES`), as a real path at
//! the floor does, so an oversized packet is dropped and counted rather than delivered. `SCHED_DIAG=1`
//! traces each run every 100 virtual ms (open exchanges by class, the congestion window, why sending
//! stopped, and the fabric's drops by cause) — the trace that found packets growing past the floor.

// Benchmark harness: an unwrap here is a failed run, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::cast_precision_loss,
  clippy::cast_possible_truncation,
  clippy::cast_sign_loss
)]

use std::sync::mpsc::{Sender, channel};

use rustls::pki_types::PrivateKeyDer;
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::{
  PPM, SimLink, SimLoss, SimPath, SimRuntime, sim_udp_add_link, sim_udp_set_pair_path,
  sim_udp_set_path,
};
use slates_rt::udp::{Ipv4Addr, SocketAddrV4, UdpSocket};
use slates_transport::congestion::ControllerKind;
use slates_transport::connection::{
  ConnectionShape, Priority, Scheduler, StreamRefusal, initial_receive_window,
};
use slates_transport::endpoint::{Endpoint, EndpointError, MAX_PACKET_PAYLOAD, MIN_DATAGRAM_BYTES};
use slates_transport::handshake::Identity;
use slates_transport::session::STREAM_FRAME_HEADER_BYTES;

/// Derived: the wire bytes each packet adds around a frame's data — the short header and AEAD tag
/// (`MIN_DATAGRAM_BYTES − MAX_PACKET_PAYLOAD`) and one `Stream` frame header — for sizing a class's load.
const WIRE_OVERHEAD: usize = MIN_DATAGRAM_BYTES - MAX_PACKET_PAYLOAD + STREAM_FRAME_HEADER_BYTES;
/// Format: the fleet's packet budget, so the bake-off frames exactly as the daemon does.
const FRAME_CAP: usize = MAX_PACKET_PAYLOAD;
/// Format: nanoseconds per millisecond and per second.
const MS: u64 = 1_000_000;
const NS_PER_SECOND: u64 = 1_000_000_000;
/// Shape: the request kinds.
const CONTROL: u64 = 1;
const METADATA: u64 = 2;
const BULK: u64 = 3;
/// Shape: a control ping's request and reply size — a small control message.
const CONTROL_BYTES: usize = 64;
/// Shape: a metadata exchange's request and reply size — three frames each way, so the class's latency
/// includes its own serialization and a scheduler that interleaves by frame is exercised.
const METADATA_BYTES: usize = 3 * FRAME_CAP;
/// Shape: the classes' shares of the link, per mille — a light control load and a moderate metadata load,
/// the rest bulk, so the link is always full and the scheduler, not idle capacity, decides the latencies.
const CONTROL_LOAD_PERMILLE: u64 = 10;
const METADATA_LOAD_PERMILLE: u64 = 50;
/// Shape: bulk transfers in flight at once — several, so a round-robin scheduler has more bulk streams
/// than other classes to cycle through (the fleet's content plane runs several puts per session).
const BULK_FLOWS: usize = 4;
/// Shape: the steady window's length in control pings — enough that the p99 is a percentile (the tenth
/// worst), not one loss recovery.
const CONTROL_SAMPLES: u64 = 1000;
/// Shape: the warm-up before the steady window, in round trips, and its floor.
const WARMUP_RTTS: u64 = 20;
const MIN_WARMUP_NS: u64 = 5 * NS_PER_SECOND;
/// Shape: the receive ceiling in BDPs — generous, so the scheduler and the controller, never flow control,
/// are the limit — and its floor in initial windows.
const CEILING_BDPS: u64 = 8;
const CEILING_FLOOR_WINDOWS: u64 = 64;
/// Shape: the seeds each scenario runs with.
const SEEDS: [u64; 3] = [1, 2, 3];
/// The schedulers in the bake-off.
const SCHEDULERS: [Scheduler; 3] = [
  Scheduler::RoundRobin,
  Scheduler::StrictPriority,
  Scheduler::Weighted,
];
const NAME: &str = "slates-bakeoff";

/// One network under test.
#[derive(Clone, Debug)]
struct Scenario {
  name: String,
  rate: u64,
  rtt_ns: u64,
  loss: SimLoss,
}

impl Scenario {
  fn bdp(&self) -> u64 {
    (u128::from(self.rate) * u128::from(self.rtt_ns) / u128::from(8 * NS_PER_SECOND)) as u64
  }

  /// The gap between exchanges of a class whose request and reply are `bytes` each (plus the packet
  /// overhead per frame) at `permille` of the link.
  fn gap_ns(&self, bytes: usize, permille: u64) -> u64 {
    let frames = bytes.div_ceil(FRAME_CAP) as u64;
    let bits = 2 * (bytes as u64 + frames * WIRE_OVERHEAD as u64) * 8;
    bits * NS_PER_SECOND * 1000 / (self.rate.max(1) * permille)
  }

  fn warmup_ns(&self) -> u64 {
    (WARMUP_RTTS * self.rtt_ns).max(MIN_WARMUP_NS)
  }

  fn window_ns(&self) -> u64 {
    CONTROL_SAMPLES * self.gap_ns(CONTROL_BYTES, CONTROL_LOAD_PERMILLE)
  }

  /// One bulk transfer: a BDP's worth or 64 kB, whichever is larger, so each completes within a few
  /// round trips and is restarted — a steady bulk load of many transfers, as the content plane has.
  fn bulk_bytes(&self) -> usize {
    usize::try_from(self.bdp().max(64 * 1024)).unwrap_or(64 * 1024)
  }
}

/// Per-class results of one run.
#[derive(Clone, Debug, Default)]
struct Outcome {
  control_ns: Vec<u64>,
  metadata_ns: Vec<u64>,
  bulk_bytes: u64,
  window_ns: u64,
  stalled: bool,
}

impl Outcome {
  fn goodput(&self) -> f64 {
    self.bulk_bytes as f64 * 8.0 / (self.window_ns.max(1) as f64 / 1e9)
  }
}

/// The `permille` percentile of `values` in milliseconds; infinite when empty.
fn percentile(values: &[u64], permille: usize) -> f64 {
  if values.is_empty() {
    return f64::INFINITY;
  }
  let mut sorted = values.to_vec();
  sorted.sort_unstable();
  let index = (sorted.len() * permille / 1000).min(sorted.len() - 1);
  sorted[index] as f64 / MS as f64
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

fn shape(scheduler: Scheduler, scenario: &Scenario) -> ConnectionShape {
  let ceiling = scenario
    .bdp()
    .saturating_mul(CEILING_BDPS)
    .max(CEILING_FLOOR_WINDOWS * initial_receive_window(FRAME_CAP));
  let mut shape = ConnectionShape::for_frame_cap(FRAME_CAP, ceiling, ControllerKind::NewReno);
  shape.scheduler = scheduler;
  shape
}

/// Runs `future` until it completes or `within_ns` of virtual time passes.
async fn within<F: std::future::Future>(within_ns: u64, future: F) -> Option<F::Output> {
  let mut future = std::pin::pin!(future);
  let mut deadline = std::pin::pin!(slates_rt::futures::sleep(within_ns));
  std::future::poll_fn(|cx| {
    if let std::task::Poll::Ready(output) = future.as_mut().poll(cx) {
      return std::task::Poll::Ready(Some(output));
    }
    if deadline.as_mut().poll(cx).is_ready() {
      return std::task::Poll::Ready(None);
    }
    std::task::Poll::Pending
  })
  .await
}

fn run(scheduler: Scheduler, scenario: &Scenario, seed: u64) -> Outcome {
  let mut sim = SimRuntime::new(&config(), seed).unwrap();
  let shard = sim.shard_ids()[0];
  let (tx, rx) = channel::<Outcome>();
  let scenario = scenario.clone();
  sim
    .spawn_on(shard, async move {
      coordinate(scheduler, scenario, tx).await;
    })
    .unwrap();
  sim.run_until_idle();
  rx.try_recv().unwrap_or(Outcome {
    stalled: true,
    ..Outcome::default()
  })
}

async fn coordinate(scheduler: Scheduler, scenario: Scenario, report: Sender<Outcome>) {
  let any = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
  let client_socket = UdpSocket::bind(any).unwrap();
  let server_socket = UdpSocket::bind(any).unwrap();
  let client_port = client_socket.local_addr().unwrap().port();
  let server_port = server_socket.local_addr().unwrap().port();
  let one_way = scenario.rtt_ns / 2;
  sim_udp_set_path(
    SimPath::in_order(one_way, 0)
      .with_loss(scenario.loss)
      .with_mtu(MIN_DATAGRAM_BYTES),
  );
  let link = sim_udp_add_link(SimLink {
    rate_bits_per_second: scenario.rate,
    queue_bytes: scenario.bdp().max(2 * MIN_DATAGRAM_BYTES as u64),
  });
  sim_udp_set_pair_path(
    client_port,
    server_port,
    SimPath::in_order(one_way, 0)
      .with_loss(scenario.loss)
      .with_mtu(MIN_DATAGRAM_BYTES)
      .through(link),
  );
  let server_identity = self_signed(NAME);
  let client_identity = self_signed(NAME);
  let server_cert = server_identity.certificate();
  let client_cert = client_identity.certificate();
  let session_shape = shape(scheduler, &scenario);
  let server = slates_rt::futures::spawn_child(async move {
    let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, client_port);
    let mut endpoint = Endpoint::server(
      server_socket,
      peer,
      &server_identity,
      &[client_cert],
      session_shape,
    )
    .unwrap();
    if endpoint.establish().await.is_err() {
      return;
    }
    loop {
      let served = endpoint
        .serve_once(|kind, request| match kind {
          BULK => (request.len() as u64).to_le_bytes().to_vec(),
          METADATA => vec![0x4D; METADATA_BYTES],
          _ => request,
        })
        .await;
      if served.is_err() {
        return;
      }
    }
  })
  .unwrap();
  let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
  let mut endpoint = Endpoint::client(
    client_socket,
    peer,
    &client_identity,
    &server_cert,
    NAME,
    session_shape,
  )
  .unwrap();
  let outcome = match within(scenario.warmup_ns(), endpoint.establish()).await {
    Some(Ok(())) => drive(&mut endpoint, &scenario).await,
    _ => Outcome {
      stalled: true,
      ..Outcome::default()
    },
  };
  let _ = report.send(outcome);
  endpoint.abandon_all();
  let _ = slates_rt::futures::cancel(server);
}

/// An open exchange: its class, when it began, and its request size.
struct Open {
  id: u64,
  kind: u64,
  began: u64,
  bytes: usize,
}

/// Runs the three classes over the warm-up and the steady window, recording the window's latencies.
async fn drive(endpoint: &mut Endpoint, scenario: &Scenario) -> Outcome {
  let start = slates_rt::futures::now_ns();
  let window_from = start.saturating_add(scenario.warmup_ns());
  let window_until = window_from.saturating_add(scenario.window_ns());
  let control_gap = scenario.gap_ns(CONTROL_BYTES, CONTROL_LOAD_PERMILLE);
  let metadata_gap = scenario.gap_ns(METADATA_BYTES, METADATA_LOAD_PERMILLE);
  let bulk = vec![0xB5; scenario.bulk_bytes()];
  let mut next_control = start;
  let mut next_metadata = start;
  let mut open: Vec<Open> = Vec::new();
  let mut last_diag = u64::MAX;
  let mut outcome = Outcome {
    window_ns: window_until - window_from,
    ..Outcome::default()
  };
  loop {
    let now = slates_rt::futures::now_ns();
    if now >= window_until {
      return outcome;
    }
    let bulk_open = open.iter().filter(|o| o.kind == BULK).count();
    let mut due: Vec<(u64, Priority, Vec<u8>)> = Vec::new();
    for _ in bulk_open..BULK_FLOWS {
      due.push((BULK, Priority::Bulk, bulk.clone()));
    }
    if now >= next_control {
      due.push((CONTROL, Priority::Control, vec![0x43; CONTROL_BYTES]));
      next_control = next_control.saturating_add(control_gap);
    }
    if now >= next_metadata {
      due.push((METADATA, Priority::Metadata, vec![0x4D; METADATA_BYTES]));
      next_metadata = next_metadata.saturating_add(metadata_gap);
    }
    for (kind, priority, request) in due {
      if let Err(e) = begin(endpoint, &mut open, kind, priority, &request, now) {
        eprintln!("RUN {}: begin {kind} refused: {e:?}", scenario.name);
        outcome.stalled = true;
        return outcome;
      }
    }
    if std::env::var_os("SCHED_DIAG").is_some() && now / (100 * MS) != last_diag {
      last_diag = now / (100 * MS);
      let by = |k: u64| open.iter().filter(|o| o.kind == k).count();
      eprintln!(
        "DIAG t={}ms open c={} m={} b={} done c={} m={} srtt={}ms cwnd={} stops={:?} fabric={:?}",
        (now - start) / MS,
        by(CONTROL),
        by(METADATA),
        by(BULK),
        outcome.control_ns.len(),
        outcome.metadata_ns.len(),
        endpoint.smoothed_rtt() / MS,
        endpoint.congestion_window(),
        endpoint.send_stops(),
        slates_rt::sim::sim_udp_stats()
      );
    }
    let wait = next_control.min(next_metadata).saturating_sub(now).max(1);
    if let Some(Err(_)) = within(wait, endpoint.drive()).await {
      outcome.stalled = true;
      return outcome;
    }
    let now = slates_rt::futures::now_ns();
    open.retain(|exchange| {
      let Some(_) = endpoint.take_reply(exchange.id) else {
        return true;
      };
      if exchange.began >= window_from && now <= window_until {
        let latency = now.saturating_sub(exchange.began);
        match exchange.kind {
          CONTROL => outcome.control_ns.push(latency),
          METADATA => outcome.metadata_ns.push(latency),
          _ => {}
        }
      }
      if exchange.kind == BULK && now > window_from && now <= window_until {
        // Bulk goodput counts every transfer completing inside the window (its bytes crossed the link
        // while the other classes were measured).
        outcome.bulk_bytes = outcome.bulk_bytes.saturating_add(exchange.bytes as u64);
      }
      false
    });
  }
}

/// Begins one exchange, recording it; a backlog refusal (the stream credit is spent) skips this one — the
/// class's next exchange comes at its next gap, and a starved class shows as no completions. Any other
/// refusal is the run's failure, returned for the caller to record.
fn begin(
  endpoint: &mut Endpoint,
  open: &mut Vec<Open>,
  kind: u64,
  priority: Priority,
  request: &[u8],
  now: u64,
) -> Result<(), EndpointError> {
  match endpoint.begin(kind, priority, request) {
    Ok(id) => {
      open.push(Open {
        id,
        kind,
        began: now,
        bytes: request.len(),
      });
      Ok(())
    }
    Err(EndpointError::Stream(StreamRefusal::Backlogged { .. })) => Ok(()),
    Err(e) => Err(e),
  }
}

fn scenarios() -> Vec<Scenario> {
  let mut all = Vec::new();
  for rate in [1_000_000, 10_000_000, 100_000_000] {
    for rtt in [20, 100] {
      for (label, loss) in [("0", SimLoss::NONE), ("1%", SimLoss::random(PPM / 100))] {
        all.push(Scenario {
          name: format!("rate={rate} rtt={rtt}ms loss={label}"),
          rate,
          rtt_ns: rtt * MS,
          loss,
        });
      }
    }
  }
  all.push(Scenario {
    name: "burst loss rate=10000000 rtt=100ms".to_owned(),
    rate: 10_000_000,
    rtt_ns: 100 * MS,
    loss: SimLoss::bursty(PPM / 400, PPM / 4, PPM),
  });
  all
}

fn main() {
  let filter = std::env::args().nth(1);
  println!(
    "scenario,scheduler,seed,control_n,control_p50_ms,control_p99_ms,control_p999_ms,metadata_n,metadata_p50_ms,metadata_p99_ms,metadata_p999_ms,bulk_mbps,capacity_share,stalled"
  );
  let mut rows: Vec<(String, Scheduler, Outcome)> = Vec::new();
  for scenario in scenarios() {
    if filter
      .as_ref()
      .is_some_and(|f| !scenario.name.contains(f.as_str()))
    {
      continue;
    }
    for scheduler in SCHEDULERS {
      for seed in SEEDS {
        let outcome = run(scheduler, &scenario, seed);
        println!(
          "{},{:?},{},{},{:.1},{:.1},{:.1},{},{:.1},{:.1},{:.1},{:.3},{:.3},{}",
          scenario.name,
          scheduler,
          seed,
          outcome.control_ns.len(),
          percentile(&outcome.control_ns, 500),
          percentile(&outcome.control_ns, 990),
          percentile(&outcome.control_ns, 999),
          outcome.metadata_ns.len(),
          percentile(&outcome.metadata_ns, 500),
          percentile(&outcome.metadata_ns, 990),
          percentile(&outcome.metadata_ns, 999),
          outcome.goodput() / 1e6,
          outcome.goodput() / scenario.rate as f64,
          outcome.stalled
        );
        rows.push((scenario.name.clone(), scheduler, outcome));
      }
    }
  }
  rank(&rows);
}

/// Applies the selection rule stated in the module doc and prints the ranking.
fn rank(rows: &[(String, Scheduler, Outcome)]) {
  let names: std::collections::BTreeSet<&String> = rows.iter().map(|(name, _, _)| name).collect();
  let count = SCHEDULERS.len();
  let mut control = vec![0.0f64; count];
  let mut metadata = vec![0.0f64; count];
  let mut goodput = vec![0.0f64; count];
  let mut worst: Vec<(f64, String)> = vec![(1.0, String::new()); count];
  let mut disqualified: Vec<String> = vec![String::new(); count];
  let mut scenarios = 0.0f64;
  for name in names {
    let mut c = vec![0.0f64; count];
    let mut m = vec![0.0f64; count];
    let mut g = vec![0.0f64; count];
    for (index, scheduler) in SCHEDULERS.iter().enumerate() {
      let runs: Vec<&Outcome> = rows
        .iter()
        .filter(|(n, s, _)| n == name && s == scheduler)
        .map(|(_, _, o)| o)
        .collect();
      for outcome in &runs {
        if outcome.stalled {
          disqualified[index] = format!("stalled in {name}");
        }
        if outcome.control_ns.is_empty()
          || outcome.metadata_ns.is_empty()
          || outcome.bulk_bytes == 0
        {
          disqualified[index] = format!("starved a class in {name}");
        }
      }
      let runs_n = runs.len().max(1) as f64;
      c[index] = runs
        .iter()
        .map(|o| percentile(&o.control_ns, 990))
        .sum::<f64>()
        / runs_n;
      m[index] = runs
        .iter()
        .map(|o| percentile(&o.metadata_ns, 990))
        .sum::<f64>()
        / runs_n;
      g[index] = runs.iter().map(|o| o.goodput()).sum::<f64>() / runs_n;
    }
    let best_c = c.iter().copied().fold(f64::INFINITY, f64::min).max(1e-9);
    let best_m = m.iter().copied().fold(f64::INFINITY, f64::min).max(1e-9);
    let best_g = g.iter().copied().fold(0.0, f64::max).max(1e-9);
    for index in 0..count {
      let ratio = c[index].max(1e-9) / best_c;
      if ratio > worst[index].0 {
        worst[index] = (ratio, name.to_string());
      }
      control[index] += ratio.ln();
      metadata[index] += (m[index].max(1e-9) / best_m).ln();
      goodput[index] += (best_g / g[index].max(1e-9)).ln();
    }
    scenarios += 1.0;
  }
  println!();
  println!(
    "scheduler,control_p99_vs_best_geomean,worst_control_p99_vs_best,worst_scenario,metadata_p99_vs_best_geomean,goodput_shortfall_vs_best_geomean,disqualified"
  );
  for (index, scheduler) in SCHEDULERS.iter().enumerate() {
    let n = scenarios.max(1.0);
    println!(
      "{:?},{:.3},{:.3},{},{:.3},{:.3},{}",
      scheduler,
      (control[index] / n).exp(),
      worst[index].0,
      worst[index].1,
      (metadata[index] / n).exp(),
      (goodput[index] / n).exp(),
      if disqualified[index].is_empty() {
        "no"
      } else {
        &disqualified[index]
      }
    );
  }
}
