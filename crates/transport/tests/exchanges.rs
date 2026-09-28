//! Concurrent, prioritized exchanges on one session, end to end (§4.10a; the constrained-link design,
//! `docs/wip/research/nfs-transport-constrained-links.md` §5.3): real endpoints — TLS 1.3 handshake,
//! packet protection, the clocked connection — over the simulated network (a bottleneck link with a
//! drop-tail queue, random and burst loss, reordering), framed at the fleet's frame cap. The fleet's
//! record session to a peer carries record commits, forwarded verbs and content transfers at once, so:
//!
//! - **head-of-line**: a control exchange must never wait behind a bulk one's unsent bytes;
//! - **deadlock**: every run is bounded by a virtual deadline, and a stall is reported with where it stood
//!   — including the stream-credit path, driven past its limit so exchanges wait on credit and resume;
//! - **leaks and unbounded growth**: once a session quiesces, both ends hold nothing — no exchange,
//!   request, stream, frame or packet ([`EndpointCensus::is_quiescent`]);
//! - **failure modes**: loss, burst loss, reordering, an abandoned exchange, a peer that dies
//!   mid-exchange — each across several seeds.
//!
//! Test by use (R5): every assertion is on what the endpoints deliver and hold.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::future::Future;
use std::sync::mpsc::{Receiver, Sender, channel};

use rustls::pki_types::PrivateKeyDer;
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::{
  PPM, SimLink, SimLoss, SimPath, SimRuntime, sim_udp_add_link, sim_udp_set_pair_path,
  sim_udp_set_path,
};
use slates_rt::udp::{Ipv4Addr, SocketAddrV4, UdpSocket};
use slates_transport::connection::{
  ConnectionShape, Priority, StreamRefusal, initial_receive_window,
};
use slates_transport::endpoint::{
  Endpoint, EndpointCensus, EndpointError, MAX_PACKET_PAYLOAD, MIN_DATAGRAM_BYTES,
};
use slates_transport::handshake::Identity;

const NAME: &str = "slates-node";
/// Format: the fleet's packet budget, so the bake-off frames exactly as the daemon does.
const FRAME_CAP: usize = MAX_PACKET_PAYLOAD;
/// Format: nanoseconds per millisecond and per second.
const MS: u64 = 1_000_000;
const NS_PER_SECOND: u64 = 1_000_000_000;
/// Shape: the request kinds the tests' server answers.
const PING: u64 = 1;
const BULK: u64 = 2;
const ECHO: u64 = 3;
/// Shape: the virtual deadline on every run — far past any healthy completion here (the slowest, a
/// 250 kB transfer at 1 Mbit/s, takes about two seconds), so reaching it means a stall.
const RUN_BOUND_NS: u64 = 120 * NS_PER_SECOND;
/// Shape: how long a drive turn waits before the loop re-checks its own schedule.
const TICK_NS: u64 = MS;
/// Shape: the seeds each lossy scenario runs with.
const SEEDS: [u64; 4] = [1, 2, 3, 4];

fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 1,
    tasks_per_shard: 64,
    timers_per_shard: 64,
    ring_entries: 64,
    step_budget_ns: 1_000_000_000,
    timer_tick_ns: 100_000,
    batch: 64,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
    wake_tracking: None,
  }
}

/// A session shape at the fleet frame cap whose receive ceiling is `windows` initial windows — which also
/// sets the stream limit (the ceiling over the frame cap, `crate::streams`).
fn shape(windows: u64) -> ConnectionShape {
  ConnectionShape::for_frame_cap(FRAME_CAP, windows * initial_receive_window(FRAME_CAP))
}

/// The stream limit a [`shape`] of `windows` gives: the receive ceiling over the frame cap.
fn stream_limit(windows: u64) -> u64 {
  windows * initial_receive_window(FRAME_CAP) / FRAME_CAP as u64
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

/// Runs `future` until it completes or `within_ns` of virtual time passes, whichever is first.
async fn within<F: Future>(within_ns: u64, future: F) -> Option<F::Output> {
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

/// The network under test: the client→server direction through an optional bottleneck link; both
/// directions with the same propagation delay, loss and reordering.
#[derive(Clone, Copy, Debug)]
struct Net {
  one_way_ns: u64,
  loss: SimLoss,
  reorder_jitter_ns: Option<u64>,
  /// The bottleneck's rate (bits per second) and drop-tail queue (bytes).
  link: Option<(u64, u64)>,
}

impl Net {
  const fn clean(one_way_ns: u64) -> Net {
    Net {
      one_way_ns,
      loss: SimLoss::NONE,
      reorder_jitter_ns: None,
      link: None,
    }
  }

  fn path(&self) -> SimPath {
    match self.reorder_jitter_ns {
      Some(jitter) => SimPath::reordering(self.one_way_ns, jitter),
      None => SimPath::in_order(self.one_way_ns, 0),
    }
    .with_loss(self.loss)
    .with_mtu(MIN_DATAGRAM_BYTES)
  }

  /// The failure-mode networks: 5 % random loss, burst loss, and reordering, on a 10 ms path.
  fn hostile() -> Vec<(&'static str, Net)> {
    let base = Net::clean(5 * MS);
    vec![
      (
        "5% random loss",
        Net {
          loss: SimLoss::random(PPM / 20),
          ..base
        },
      ),
      (
        "burst loss",
        Net {
          loss: SimLoss::bursty(PPM / 100, PPM / 4, PPM),
          ..base
        },
      ),
      (
        "reordering",
        Net {
          reorder_jitter_ns: Some(4 * MS),
          ..base
        },
      ),
    ]
  }
}

/// How the server behaves.
#[derive(Clone, Copy, Debug)]
enum ServerMode {
  /// Serves every request until the client is done, then settles.
  Serve,
  /// Takes `n` requests without replying, then drops the session — a peer that dies mid-exchange.
  DieAfter(u32),
}

/// What the server reports when its session ends.
#[derive(Debug)]
struct ServerReport {
  census: EndpointCensus,
  violations: u64,
  served: u32,
  settled: bool,
}

/// What one run produced.
#[derive(Debug)]
struct Report<T> {
  client: Result<T, String>,
  client_census: EndpointCensus,
  client_violations: u64,
  client_settled: bool,
  /// The client's settle was cut off at the harness's bound (its peer was gone while still owed a reset).
  client_settle_cut_off: bool,
  server: Result<ServerReport, String>,
}

/// The server's answer: a ping echoed, a bulk transfer acknowledged with its length, anything else echoed
/// with every byte offset by its kind — a transform, so a crossed reply would show.
fn answer(kind: u64, request: Vec<u8>) -> Vec<u8> {
  match kind {
    BULK => (request.len() as u64).to_le_bytes().to_vec(),
    PING => request,
    _ => {
      let offset = u8::try_from(kind & 0xFF).unwrap_or(0);
      request.iter().map(|b| b.wrapping_add(offset)).collect()
    }
  }
}

/// Runs one session over `net`: the client runs `client_work` and the harness then keeps its session
/// driven until the server has settled and reported, so every final acknowledgement is delivered; each
/// side then settles and reports what it still holds. Everything is bounded by [`RUN_BOUND_NS`].
fn run_session<T, W, F>(
  seed: u64,
  net: Net,
  windows: u64,
  mode: ServerMode,
  client_work: W,
) -> Report<T>
where
  T: Send + 'static,
  W: FnOnce(Endpoint) -> F + Send + 'static,
  F: Future<Output = (Endpoint, Result<T, String>)> + 'static,
{
  let mut sim = SimRuntime::new(&config(), seed).unwrap();
  let shard = sim.shard_ids()[0];
  let (report_tx, report_rx) = channel::<Report<T>>();
  sim
    .spawn_on(shard, async move {
      coordinate(net, windows, mode, client_work, report_tx).await;
    })
    .unwrap();
  sim.run_until_idle();
  report_rx
    .try_recv()
    .expect("the run reported (a missing report is a stall the deadline did not catch)")
}

async fn coordinate<T, W, F>(
  net: Net,
  windows: u64,
  mode: ServerMode,
  client_work: W,
  report_tx: Sender<Report<T>>,
) where
  T: Send + 'static,
  W: FnOnce(Endpoint) -> F + 'static,
  F: Future<Output = (Endpoint, Result<T, String>)> + 'static,
{
  let any = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
  let client_socket = UdpSocket::bind(any).unwrap();
  let server_socket = UdpSocket::bind(any).unwrap();
  let client_port = client_socket.local_addr().unwrap().port();
  let server_port = server_socket.local_addr().unwrap().port();
  sim_udp_set_path(net.path());
  if let Some((rate, queue_bytes)) = net.link {
    let link = sim_udp_add_link(SimLink {
      rate_bits_per_second: rate,
      queue_bytes,
    });
    sim_udp_set_pair_path(client_port, server_port, net.path().through(link));
  }
  let server_identity = self_signed(NAME);
  let client_identity = self_signed(NAME);
  let server_cert = server_identity.certificate();
  let client_cert = client_identity.certificate();
  let (done_tx, done_rx) = channel::<()>();
  let (server_tx, server_rx) = channel::<Result<ServerReport, String>>();
  let server = slates_rt::futures::spawn_child(async move {
    let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, client_port);
    let outcome = match Endpoint::server(
      server_socket,
      peer,
      &server_identity,
      &[client_cert],
      shape(windows),
    ) {
      Ok(endpoint) => serve(endpoint, mode, done_rx).await,
      Err(e) => Err(format!("server: {e:?}")),
    };
    let _ = server_tx.send(outcome);
  })
  .unwrap();
  let client = slates_rt::futures::spawn_child(async move {
    let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
    let report = match Endpoint::client(
      client_socket,
      peer,
      &client_identity,
      &server_cert,
      NAME,
      shape(windows),
    ) {
      Ok(endpoint) => drive_client(endpoint, client_work, done_tx, server_rx).await,
      Err(e) => Report {
        client: Err(format!("client: {e:?}")),
        client_census: EndpointCensus::default(),
        client_violations: 0,
        client_settled: false,
        client_settle_cut_off: false,
        server: Err("never ran".to_owned()),
      },
    };
    let _ = report_tx.send(report);
  })
  .unwrap();
  let _ = slates_rt::futures::join(client).await;
  let _ = slates_rt::futures::join(server).await;
}

async fn drive_client<T, W, F>(
  mut endpoint: Endpoint,
  client_work: W,
  done_tx: Sender<()>,
  server_rx: Receiver<Result<ServerReport, String>>,
) -> Report<T>
where
  W: FnOnce(Endpoint) -> F,
  F: Future<Output = (Endpoint, Result<T, String>)>,
{
  let started = slates_rt::futures::now_ns();
  let client = match within(RUN_BOUND_NS, endpoint.establish()).await {
    Some(Ok(())) => {
      let (returned, outcome) = match within(RUN_BOUND_NS, client_work(endpoint)).await {
        Some(pair) => pair,
        None => return stalled("the client's work", started),
      };
      endpoint = returned;
      outcome
    }
    Some(Err(e)) => Err(format!("client establish: {e:?}")),
    None => return stalled("the client's handshake", started),
  };
  let _ = done_tx.send(());
  let server = linger(&mut endpoint, &server_rx).await;
  // Against a live peer this settles; against a dead one still owed something it never can, so the wait is
  // bounded here — as every caller bounds it — across many probe timeouts of silence.
  let dead_peer = server.as_ref().is_ok_and(|report| !report.settled);
  let settle_bound = if dead_peer {
    DEAD_PEER_SILENCE_NS
  } else {
    RUN_BOUND_NS
  };
  let client_settle = within(settle_bound, endpoint.settle()).await;
  let client_settled = matches!(client_settle, Some(Ok(())));
  let client_settle_cut_off = client_settle.is_none();
  if client_settled {
    let _ = within(RUN_BOUND_NS, drive_to_quiescence(&mut endpoint)).await;
  }
  Report {
    client,
    client_census: endpoint.census(),
    client_violations: endpoint.protocol_violations(),
    client_settled,
    client_settle_cut_off,
    server,
  }
}

/// Drives `endpoint` until it holds nothing ([`EndpointCensus::is_quiescent`]); unbounded itself, so the
/// caller races it against a deadline.
async fn drive_to_quiescence(endpoint: &mut Endpoint) {
  while !endpoint.census().is_quiescent() {
    if let Some(Err(_)) = within(TICK_NS, endpoint.drive()).await {
      return;
    }
  }
}

/// Keeps the client's session driven until the server has settled and reported, so its final replies and
/// credit are acknowledged; bounded, since a dead server never reports.
async fn linger(
  endpoint: &mut Endpoint,
  server_rx: &Receiver<Result<ServerReport, String>>,
) -> Result<ServerReport, String> {
  let linger_until = slates_rt::futures::now_ns().saturating_add(RUN_BOUND_NS);
  while slates_rt::futures::now_ns() < linger_until {
    if let Ok(report) = server_rx.try_recv() {
      return report;
    }
    if let Some(Err(e)) = within(TICK_NS, endpoint.drive()).await {
      return Err(format!("client drive while lingering: {e:?}"));
    }
  }
  Err("the server never reported".to_owned())
}

fn stalled<T>(stage: &str, started: u64) -> Report<T> {
  Report {
    client: Err(format!(
      "stalled in {stage}: no progress by {} virtual seconds",
      slates_rt::futures::now_ns().saturating_sub(started) / NS_PER_SECOND
    )),
    client_census: EndpointCensus::default(),
    client_violations: 0,
    client_settled: false,
    client_settle_cut_off: false,
    server: Err("stalled".to_owned()),
  }
}

async fn serve(
  mut endpoint: Endpoint,
  mode: ServerMode,
  done_rx: Receiver<()>,
) -> Result<ServerReport, String> {
  match within(RUN_BOUND_NS, endpoint.establish()).await {
    Some(Ok(())) => {}
    Some(Err(e)) => return Err(format!("server establish: {e:?}")),
    None => return Err("server handshake stalled".to_owned()),
  }
  let mut served = 0u32;
  let deadline = slates_rt::futures::now_ns().saturating_add(RUN_BOUND_NS);
  while slates_rt::futures::now_ns() < deadline {
    if let ServerMode::DieAfter(n) = mode
      && served >= n
    {
      // Dies: the session is dropped with the requests unanswered.
      return Ok(ServerReport {
        census: endpoint.census(),
        violations: endpoint.protocol_violations(),
        served,
        settled: false,
      });
    }
    if done_rx.try_recv().is_ok() {
      let settled = matches!(within(RUN_BOUND_NS, endpoint.settle()).await, Some(Ok(())));
      // Settled means the peer is owed nothing; a credit frame may still be in flight. Drive on until
      // this end holds nothing at all (the client is still lingering, so it is acknowledged), so the leak
      // check below is "nothing held", not merely "nothing owed".
      let _ = within(RUN_BOUND_NS, drive_to_quiescence(&mut endpoint)).await;
      return Ok(ServerReport {
        census: endpoint.census(),
        violations: endpoint.protocol_violations(),
        served,
        settled,
      });
    }
    let turn = match mode {
      ServerMode::Serve => within(TICK_NS, endpoint.serve_once(answer)).await,
      ServerMode::DieAfter(_) => {
        within(TICK_NS, async { endpoint.next_request().await.map(|_| ()) }).await
      }
    };
    match turn {
      Some(Ok(())) => served += 1,
      Some(Err(e)) => return Err(format!("server serve: {e:?}")),
      None => {}
    }
  }
  Err("the server's serve loop reached its deadline".to_owned())
}

/// Asserts a healthy run: the client's work succeeded, neither end saw a protocol violation, both settled,
/// and both hold nothing.
fn assert_clean<T: std::fmt::Debug>(label: &str, report: &Report<T>) {
  assert!(
    report.client.is_ok(),
    "{label}: the client failed: {report:?}"
  );
  let server = report
    .server
    .as_ref()
    .unwrap_or_else(|e| panic!("{label}: the server failed: {e}"));
  assert_eq!(report.client_violations, 0, "{label}: client violations");
  assert_eq!(server.violations, 0, "{label}: server violations");
  assert!(server.settled, "{label}: the server settled: {server:?}");
  assert!(report.client_settled, "{label}: the client settled");
  assert!(
    server.census.is_quiescent(),
    "{label}: the server leaked: {:?}",
    server.census
  );
  assert!(
    report.client_census.is_quiescent(),
    "{label}: the client leaked: {:?}",
    report.client_census
  );
}

/// Shape: the head-of-line scenario's bottleneck — 1 Mbit/s, a 40 ms round trip, a queue of one BDP.
const HOL_RATE: u64 = 1_000_000;
const HOL_ONE_WAY_NS: u64 = 20 * MS;
/// Derived: one bandwidth-delay product of the head-of-line link, bytes.
const HOL_BDP_BYTES: u64 = HOL_RATE * 2 * HOL_ONE_WAY_NS / (8 * NS_PER_SECOND);
/// Shape: the bulk transfer — two seconds of the link's capacity.
const HOL_BULK_BYTES: usize = (2 * HOL_RATE / 8) as usize;
/// Shape: the pings, each a small control request at a gap, all while the bulk transfer loads the link.
const HOL_PINGS: u64 = 12;
const HOL_PING_BYTES: usize = 64;
const HOL_PING_GAP_NS: u64 = 100 * MS;
/// Shape: the first ping waits until the bulk flow has filled the queue.
const HOL_PING_START_NS: u64 = 300 * MS;

/// What the head-of-line client measured.
#[derive(Debug)]
struct HolOutcome {
  ping_latencies_ns: Vec<u64>,
  last_ping_done_ns: u64,
  bulk_done_ns: u64,
}

/// AC (§4.10a; research note §5.3 — the constrained-link design's priority classes): on **one** session
/// across a 1 Mbit/s bottleneck whose queue a bulk transfer keeps full, a control exchange completes in
/// about one round trip plus the queue it cannot jump — never behind the transfer's unsent bytes. Do X
/// (start a 250 kB bulk exchange, then twelve control pings on the same session), expect Y (every ping's
/// latency ≤ RTT + one full queue's drain + four packets' serialization; the bulk transfer still running
/// when the last ping completes — non-vacuous: the pings really overlapped it; both ends quiescent after).
/// The single-exchange session this replaced made every ping wait for the whole transfer (~2 s).
#[test]
fn a_control_exchange_is_not_queued_behind_a_bulk_one_on_the_same_session() {
  let net = Net {
    link: Some((HOL_RATE, HOL_BDP_BYTES)),
    ..Net::clean(HOL_ONE_WAY_NS)
  };
  let serialization_ns = MIN_DATAGRAM_BYTES as u64 * 8 * NS_PER_SECOND / HOL_RATE;
  let queue_drain_ns = HOL_BDP_BYTES * 8 * NS_PER_SECOND / HOL_RATE;
  let bound_ns = 2 * HOL_ONE_WAY_NS + queue_drain_ns + 4 * serialization_ns;
  for seed in [1, 2, 3] {
    let report = run_session(seed, net, 64, ServerMode::Serve, hol_client);
    assert_clean(&format!("seed {seed}"), &report);
    let outcome = report.client.as_ref().unwrap();
    assert_eq!(outcome.ping_latencies_ns.len() as u64, HOL_PINGS);
    assert!(
      outcome.bulk_done_ns > outcome.last_ping_done_ns,
      "seed {seed}: the pings overlapped the transfer: {outcome:?}"
    );
    let worst = outcome.ping_latencies_ns.iter().copied().max().unwrap_or(0);
    assert!(
      worst <= bound_ns,
      "seed {seed}: the worst ping took {} ms, past the {} ms bound (RTT + queue + 4 packets): {:?}",
      worst / MS,
      bound_ns / MS,
      outcome.ping_latencies_ns
    );
  }
}

async fn hol_client(mut endpoint: Endpoint) -> (Endpoint, Result<HolOutcome, String>) {
  let outcome = hol_work(&mut endpoint).await;
  (endpoint, outcome)
}

async fn hol_work(endpoint: &mut Endpoint) -> Result<HolOutcome, String> {
  let start = slates_rt::futures::now_ns();
  let bulk = endpoint
    .begin(BULK, Priority::Bulk, &vec![0xB5; HOL_BULK_BYTES])
    .map_err(|e| format!("begin bulk: {e:?}"))?;
  let mut bulk_done_ns = None;
  let mut pings: Vec<(u64, u64)> = Vec::new();
  let mut latencies = Vec::new();
  let mut last_ping_done_ns = 0;
  let mut next_ping = start.saturating_add(HOL_PING_START_NS);
  let mut sent = 0u64;
  while bulk_done_ns.is_none() || latencies.len() as u64 != HOL_PINGS {
    let now = slates_rt::futures::now_ns();
    if sent < HOL_PINGS && now >= next_ping {
      let id = endpoint
        .begin(PING, Priority::Control, &[0x50; HOL_PING_BYTES])
        .map_err(|e| format!("begin ping: {e:?}"))?;
      pings.push((id, now));
      sent += 1;
      next_ping = next_ping.saturating_add(HOL_PING_GAP_NS);
    }
    let wait = if sent < HOL_PINGS {
      next_ping.saturating_sub(now).max(1)
    } else {
      TICK_NS
    };
    if let Some(Err(e)) = within(wait, endpoint.drive()).await {
      return Err(format!("drive: {e:?}"));
    }
    let now = slates_rt::futures::now_ns();
    let mut still = Vec::new();
    for (id, began) in pings.drain(..) {
      match endpoint.take_reply(id) {
        Some(reply) if reply.len() == HOL_PING_BYTES => {
          latencies.push(now.saturating_sub(began));
          last_ping_done_ns = now.saturating_sub(start);
        }
        Some(reply) => return Err(format!("a ping echoed {} bytes", reply.len())),
        None => still.push((id, began)),
      }
    }
    pings = still;
    if bulk_done_ns.is_none()
      && let Some(reply) = endpoint.take_reply(bulk)
    {
      if reply != (HOL_BULK_BYTES as u64).to_le_bytes() {
        return Err(format!("the bulk acknowledgement was {reply:?}"));
      }
      bulk_done_ns = Some(now.saturating_sub(start));
    }
  }
  Ok(HolOutcome {
    ping_latencies_ns: latencies,
    last_ping_done_ns,
    bulk_done_ns: bulk_done_ns.unwrap_or(0),
  })
}

/// Shape: a small receive ceiling, so the stream limit is small and the credit path runs many times.
const CREDIT_WINDOWS: u64 = 4;
/// Shape: the concurrent exchanges — several times the stream limit.
const CREDIT_EXCHANGES: u64 = 5 * CREDIT_WINDOWS * 4;

/// What the concurrent client observed.
#[derive(Debug)]
struct ConcurrentOutcome {
  completed: u64,
  backlog_refusals: u64,
  wrong: Vec<u64>,
}

/// AC (§4.10a; RFC 9000 §4.6 stream concurrency): many exchanges of every class and several sizes run at
/// once on one session — past the stream limit, so the client meets the typed backlog refusal and waits on
/// credit — over lossy, bursty and reordering paths, every seed. Do X (begin all, as fast as the refusals
/// allow, and drive), expect Y (every reply is exactly its request's transform, no protocol violation on
/// either end — a sender keeping to its credit never trips the receiver — and both ends hold nothing once
/// the session quiesces). Non-vacuous: the backlog refusal was met (credit really ran out and came back).
#[test]
fn concurrent_exchanges_past_the_stream_limit_survive_every_hostile_path_and_leak_nothing() {
  assert!(CREDIT_EXCHANGES > 2 * stream_limit(CREDIT_WINDOWS));
  for (label, net) in Net::hostile() {
    for seed in SEEDS {
      let report = run_session(
        seed,
        net,
        CREDIT_WINDOWS,
        ServerMode::Serve,
        concurrent_client,
      );
      let label = format!("{label}, seed {seed}");
      assert_clean(&label, &report);
      let outcome = report.client.as_ref().unwrap();
      assert_eq!(outcome.completed, CREDIT_EXCHANGES, "{label}");
      assert!(
        outcome.wrong.is_empty(),
        "{label}: wrong replies {:?}",
        outcome.wrong
      );
      assert!(
        outcome.backlog_refusals > 0,
        "{label}: the stream credit never ran out — the test did not reach the limit"
      );
    }
  }
}

/// The `index`-th request: a size from one byte to three frames, and a fingerprint.
fn request_of(index: u64) -> Vec<u8> {
  let len = usize::try_from(index * 37 % (3 * FRAME_CAP as u64) + 1).unwrap_or(1);
  (0..len)
    .map(|at| u8::try_from((index.saturating_add(at as u64)) % 251).unwrap_or(0))
    .collect()
}

fn class_of(index: u64) -> Priority {
  match index % 3 {
    0 => Priority::Control,
    1 => Priority::Metadata,
    _ => Priority::Bulk,
  }
}

async fn concurrent_client(
  mut endpoint: Endpoint,
) -> (Endpoint, Result<ConcurrentOutcome, String>) {
  let outcome = concurrent_work(&mut endpoint).await;
  (endpoint, outcome)
}

async fn concurrent_work(endpoint: &mut Endpoint) -> Result<ConcurrentOutcome, String> {
  let mut pending: Vec<(u64, u64)> = Vec::new();
  let mut next = 0u64;
  let mut outcome = ConcurrentOutcome {
    completed: 0,
    backlog_refusals: 0,
    wrong: Vec::new(),
  };
  while outcome.completed < CREDIT_EXCHANGES {
    while next < CREDIT_EXCHANGES {
      match endpoint.begin(ECHO, class_of(next), &request_of(next)) {
        Ok(id) => {
          pending.push((id, next));
          next += 1;
        }
        Err(EndpointError::Stream(StreamRefusal::Backlogged { .. })) => {
          outcome.backlog_refusals += 1;
          break;
        }
        Err(e) => return Err(format!("begin {next}: {e:?}")),
      }
    }
    if let Some(Err(e)) = within(TICK_NS, endpoint.drive()).await {
      return Err(format!("drive: {e:?}"));
    }
    let mut still = Vec::new();
    for (id, index) in pending.drain(..) {
      match endpoint.take_reply(id) {
        Some(reply) => {
          if reply != answer(ECHO, request_of(index)) {
            outcome.wrong.push(index);
          }
          outcome.completed += 1;
        }
        None => still.push((id, index)),
      }
    }
    pending = still;
  }
  Ok(outcome)
}

/// AC (§4.10a; RFC 9000 §3.1, §3.5 — reset and stop-sending): an exchange abandoned mid-transfer frees
/// everything on **both** ends — the client's request stops (reset) and its reply is refused (stop
/// sending), the server discards the half-arrived request and returns its credit — and the session goes
/// on carrying other exchanges. Do X (begin a large transfer, let part of it cross a lossy path, abandon
/// it, then run a ping), expect Y (the ping completes, the server never served the abandoned request, and
/// both ends hold nothing once the session quiesces — no half-arrived request left behind).
#[test]
fn an_abandoned_exchange_frees_everything_on_both_ends() {
  for (label, net) in Net::hostile() {
    for seed in SEEDS {
      let report = run_session(seed, net, 64, ServerMode::Serve, abandoning_client);
      let label = format!("{label}, seed {seed}");
      assert_clean(&label, &report);
      let server = report.server.as_ref().unwrap();
      assert_eq!(server.served, 1, "{label}: only the ping was served");
    }
  }
}

/// Shape: the abandoned transfer, and how long it runs before it is abandoned.
const ABANDONED_BYTES: usize = 64 * FRAME_CAP;
const ABANDON_AFTER_NS: u64 = 30 * MS;

async fn abandoning_client(mut endpoint: Endpoint) -> (Endpoint, Result<(), String>) {
  let outcome = abandon_work(&mut endpoint).await;
  (endpoint, outcome)
}

async fn abandon_work(endpoint: &mut Endpoint) -> Result<(), String> {
  let transfer = endpoint
    .begin(BULK, Priority::Bulk, &vec![0x7E; ABANDONED_BYTES])
    .map_err(|e| format!("begin: {e:?}"))?;
  let until = slates_rt::futures::now_ns().saturating_add(ABANDON_AFTER_NS);
  while slates_rt::futures::now_ns() < until {
    if let Some(Err(e)) = within(TICK_NS, endpoint.drive()).await {
      return Err(format!("drive: {e:?}"));
    }
  }
  if endpoint.take_reply(transfer).is_some() {
    return Err("the transfer completed before it could be abandoned".to_owned());
  }
  endpoint.abandon(transfer);
  let reply = endpoint
    .request(PING, Priority::Control, &[1, 2, 3])
    .await
    .map_err(|e| format!("ping: {e:?}"))?;
  if reply != [1, 2, 3] {
    return Err(format!("the ping echoed {reply:?}"));
  }
  Ok(())
}

/// Shape: how long the client waits for a reply from a peer that has died.
const DEAD_PEER_DEADLINE_NS: u64 = 500 * MS;

/// Shape: how long the client keeps settling toward a dead peer before the harness cuts the wait off —
/// thirty seconds, hundreds of probe timeouts on this 10 ms path, so unbounded growth would show.
const DEAD_PEER_SILENCE_NS: u64 = 30 * NS_PER_SECOND;
/// Format: the most packets a dead peer can leave tracked here — the reset and the stop-sending an
/// abandoned exchange owes (at most two original packets) plus the two most recent probe copies the
/// connection keeps (`Connection::queue_probe_copy`).
const DEAD_PEER_TRACKED_BOUND: usize = 4;

/// AC (§4.8 — every reliable wait is bounded by its caller; RFC 9002 §6.2.4 — probes are copies): a peer
/// that dies never replies; the client's deadline ends the wait and abandoning the exchange leaves no
/// exchange or stream state behind locally. What the client still owes the dead peer decides its settle —
/// both cases, each exactly:
/// - the peer died **after** acknowledging the request: nothing is owed (the reset of a delivered request
///   is not needed), so the client settles at once;
/// - the peer died **before** hearing the request: the client's reset is owed and can never be
///   acknowledged, so settling never completes — the caller's bound ends it — and across thirty seconds of
///   silence (hundreds of probe timeouts) the tracked packets stay within the originals plus one copy each.
///
/// Do X (the server drops its session after taking one request, or before any), expect Y (the request is
/// cut off at its deadline, the settle ends as above, no exchange or stream is left open, and tracking is
/// bounded).
#[test]
fn a_peer_that_dies_mid_exchange_is_bounded_by_the_deadline_and_leaves_no_exchange_behind() {
  for (took, owed_forever) in [(1, false), (0, true)] {
    for seed in SEEDS {
      let label = format!("dies after {took} request(s), seed {seed}");
      let report = run_session(
        seed,
        Net::clean(5 * MS),
        64,
        ServerMode::DieAfter(took),
        dead_peer_client,
      );
      assert_eq!(
        report.client,
        Ok(true),
        "{label}: the deadline cut the wait off"
      );
      let server = report.server.as_ref().unwrap();
      assert!(!server.settled, "{label}: the server died unsettled");
      assert_eq!(
        (report.client_settled, report.client_settle_cut_off),
        (!owed_forever, owed_forever),
        "{label}: how the client's settle ended"
      );
      let census = report.client_census;
      assert_eq!(census.exchanges, 0, "{label}: {census:?}");
      assert_eq!(
        (
          census.connection.streams.local_open,
          census.connection.send_streams,
          census.connection.recv_streams,
          census.connection.unacked
        ),
        (0, 0, 0, 0),
        "{label}: no stream state is left behind: {census:?}"
      );
      assert!(
        census.connection.in_flight <= DEAD_PEER_TRACKED_BOUND,
        "{label}: tracking toward the dead peer stayed bounded: {census:?}"
      );
    }
  }
}

async fn dead_peer_client(mut endpoint: Endpoint) -> (Endpoint, Result<bool, String>) {
  let cut_off = within(
    DEAD_PEER_DEADLINE_NS,
    endpoint.request(ECHO, Priority::Control, b"are you there"),
  )
  .await
  .is_none();
  if let Some(id) = endpoint.last_exchange() {
    endpoint.abandon(id);
  }
  (endpoint, Ok(cut_off))
}
