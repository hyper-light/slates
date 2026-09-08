//! The SWIM failure detector, live over the simulated UDP fabric (§4.8): a prober ships a probe to a
//! peer over the fleet transport and folds the acknowledgement (and its piggybacked gossip) back into
//! the detector; a peer that does not answer within the deadline is a real timeout that drives the
//! detector to suspicion. "Production endpoints and detector logic over simulated UDP" — one process,
//! two nodes, real mutually-authenticated sessions; real network/process deployment is a further gate.
//! The suspect → dead aging itself is proven sans-io in the detector's own tests; here we prove the two
//! live integration points: a real acknowledgement keeps a peer alive, and a real timeout suspects it.
//! Test by use (R5).

// Test harness: an unwrap here is a failed test.
#![allow(clippy::unwrap_used)]

use std::sync::mpsc::{Receiver, channel};

use rustix::net::{Ipv4Addr, SocketAddrV4};
use rustls::pki_types::PrivateKeyDer;
use slates_cluster::CommitBudget;
use slates_cluster::detector::{Detector, DetectorTiming};
use slates_cluster::membership::{Liveness, MemberState};
use slates_cluster::swim::{ProbeOutcome, SwimMessage, probe_once, serve_probe};
use slates_db::register::HostId;
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::SimRuntime;
use slates_rt::udp::UdpSocket;
use slates_transport::endpoint::Endpoint;
use slates_transport::handshake::Identity;

const NAME: &str = "slates-node";
const FRAME_CAP: usize = 16;
const PROBER: HostId = HostId(1);
const TARGET: HostId = HostId(2);
// A change the target already knows, seeded so its acknowledgement carries gossip the prober learns.
const RUMOUR: HostId = HostId(6);
const GOSSIP_FANOUT: usize = 8;
// Test values; a production caller derives the deadline from a measured RTT budget (owed).
const DEADLINE_NS: u64 = 20_000_000;
const POLL_NS: u64 = 1_000;

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

async fn recv_port(rx: Receiver<u16>) -> u16 {
  loop {
    if let Ok(p) = rx.try_recv() {
      return p;
    }
    slates_rt::futures::sleep(1_000).await;
  }
}

fn timing() -> DetectorTiming {
  DetectorTiming {
    // A two-period window so an unanswered probe reaches Suspect on the resolving tick and would need a
    // further period to reach Dead — the live test observes the suspicion step; the Lifeguard multiplier
    // is off (health_max 0) so the timing is exact. (The suspect → dead aging is proven in the detector.)
    suspicion_periods: 2,
    gossip_transmits: 3,
    health_max: 0,
  }
}

fn budget() -> CommitBudget {
  CommitBudget {
    deadline_ns: DEADLINE_NS,
    poll_interval_ns: POLL_NS,
  }
}

/// What one live probe round produced at the prober: whether it timed out, the gossip any acknowledgement
/// carried, and the prober's belief about the target after resolving the probe with one further tick.
struct ProbeResult {
  timed_out: bool,
  ack_gossip: Vec<(HostId, MemberState)>,
  target_after_resolution: Option<Liveness>,
}

/// Runs one live probe round: the prober (`1`) probes the target (`2`) over a mutually-authenticated
/// session. When `target_serves`, the target serves the probe (seeding a rumour so its acknowledgement
/// carries gossip); otherwise it handshakes and leaves without answering, so the probe must time out.
/// The prober ticks once to set up the probe, probes, then ticks again to resolve it (an acknowledged
/// probe keeps the target alive; an unanswered one suspects it).
fn run_probe(target_serves: bool) -> ProbeResult {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];

  let prober_identity = self_signed(NAME);
  let prober_cert = prober_identity.certificate();
  let target_identity = self_signed(NAME);
  let target_cert = target_identity.certificate();

  let (prober_port_tx, prober_port_rx) = channel::<u16>();
  let (target_port_tx, target_port_rx) = channel::<u16>();
  let (result_tx, result_rx) = channel::<ProbeResult>();

  // The target: handshake, then either serve one probe or leave without answering.
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = target_port_tx.send(socket.local_addr().unwrap().port());
      let prober_port = recv_port(prober_port_rx).await;
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, prober_port);
      let mut endpoint = Endpoint::server(
        socket,
        peer,
        &target_identity,
        std::slice::from_ref(&prober_cert),
        FRAME_CAP,
      )
      .unwrap();
      endpoint.establish().await.unwrap();
      if target_serves {
        let mut detector = Detector::new(TARGET, timing());
        // A rumour the target already holds, so its acknowledgement piggybacks gossip.
        detector.apply(
          RUMOUR,
          MemberState {
            liveness: Liveness::Suspect,
            incarnation: 1,
          },
        );
        serve_probe(&mut endpoint, &mut detector, TARGET, GOSSIP_FANOUT)
          .await
          .unwrap();
      }
      // An unserved target handshakes then leaves; the prober's probe must not block on it.
    })
    .unwrap();

  // The prober: dial the target, probe it, and report what the round produced.
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = prober_port_tx.send(socket.local_addr().unwrap().port());
      let target_port = recv_port(target_port_rx).await;
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, target_port);
      let mut endpoint = Endpoint::client(
        socket,
        peer,
        &prober_identity,
        &target_cert,
        NAME,
        FRAME_CAP,
      )
      .unwrap();
      endpoint.establish().await.unwrap();

      let mut detector = Detector::new(PROBER, timing());
      detector.join(TARGET);
      // Tick to open the probe of the target (sets it as this period's probe target).
      let _ = detector.tick();
      let ping = SwimMessage::Ping {
        from: PROBER,
        gossip: detector.gossip(GOSSIP_FANOUT),
      };
      let (_endpoint, outcome) = probe_once(endpoint, &ping, budget()).await.unwrap();

      let (timed_out, ack_gossip) = match outcome {
        ProbeOutcome::Acked(gossip) => {
          detector.on_ack(TARGET);
          detector.apply_gossip(&gossip);
          (false, gossip)
        }
        ProbeOutcome::TimedOut => (true, Vec::new()),
      };
      // Resolve the probe: an acknowledged target stays alive, an unanswered one is suspected.
      let _ = detector.tick();
      let target_after_resolution = detector.membership().state(TARGET).map(|s| s.liveness);

      let _ = result_tx.send(ProbeResult {
        timed_out,
        ack_gossip,
        target_after_resolution,
      });
    })
    .unwrap();

  sim.run_until_idle();
  result_rx.try_recv().unwrap()
}

/// A live probe that the target answers is acknowledged within the deadline, carries the target's
/// gossip back to the prober, and keeps the target alive — the happy path of the failure detector over
/// real sessions.
#[test]
fn a_live_probe_is_acknowledged_and_carries_gossip() {
  let result = run_probe(true);
  assert!(!result.timed_out, "the target answered within the deadline");
  assert_eq!(
    result.target_after_resolution,
    Some(Liveness::Alive),
    "an acknowledged target stays alive"
  );
  assert!(
    result.ack_gossip.contains(&(
      RUMOUR,
      MemberState {
        liveness: Liveness::Suspect,
        incarnation: 1,
      }
    )),
    "the acknowledgement carried the target's gossip to the prober"
  );
}

/// A live probe to a peer that never answers really times out at the deadline (it does not hang the
/// prober), and the detector then suspects it — the failure path over real sessions.
#[test]
fn a_probe_to_a_silent_peer_times_out_and_is_suspected() {
  let result = run_probe(false);
  assert!(
    result.timed_out,
    "an unanswered probe times out at the deadline"
  );
  assert_eq!(
    result.target_after_resolution,
    Some(Liveness::Suspect),
    "a timed-out probe drives the target to suspicion"
  );
}
