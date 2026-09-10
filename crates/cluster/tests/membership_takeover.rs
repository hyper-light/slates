//! The live membership → takeover path end to end over the simulated UDP fabric (§4.8): a survivor
//! probes a peer it backs over a real mutually-authenticated session; the peer is silent (it completes
//! the handshake, then never answers a probe), so the real timeout drives the survivor's failure
//! detector to declare it dead; `sync_membership` folds that converged view into the survivor's
//! `FleetNode`, which retires the dead peer and hands the survivor the peer's objects that rendezvous
//! now ranks first to it. This joins the pieces proven separately — the SWIM probe over the transport
//! (`swim.rs`), the detector's suspect→dead aging (the detector's own tests), the detector→fleet bridge
//! and the routing takeover (`fleet.rs`) — into the whole live path a fleet node's control-shard loop
//! runs. Test by use (R5); real multi-node process deployment is a further gate.

// Test harness: an unwrap here is a failed test.
#![allow(clippy::unwrap_used)]

use std::sync::mpsc::{Receiver, channel};

use rustix::net::{Ipv4Addr, SocketAddrV4};
use rustls::pki_types::PrivateKeyDer;
use slates_cluster::CommitBudget;
use slates_cluster::detector::{Detector, DetectorTiming};
use slates_cluster::fleet::{FleetNode, sync_membership};
use slates_cluster::swim::{ProbeOutcome, SwimMessage, probe_once};
use slates_db::register::{HostId, ObjectId, Quorum};
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::SimRuntime;
use slates_rt::udp::UdpSocket;
use slates_transport::endpoint::Endpoint;
use slates_transport::handshake::Identity;

const NAME: &str = "slates-node";
const FRAME_CAP: usize = 16;
const SURVIVOR: HostId = HostId(1);
const DEAD: HostId = HostId(2);
const GOSSIP_FANOUT: usize = 8;
/// Shape: the probe deadline (ns); a silent peer must time out well within the sim's step budget.
const DEADLINE_NS: u64 = 20_000_000;
/// Shape: the probe reply poll interval (ns).
const POLL_NS: u64 = 1_000;
/// Shape: how many objects of the dead peer the survivor backs — enough that some rendezvous-rank to it.
const BACKED_OBJECTS: u64 = 64;
/// Shape: a bound on the detector ticks that age the suspicion to death — well above the two-period
/// window below, so the loop declares the silent peer dead but never spins unbounded.
const TICK_CEILING: usize = 32;

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

/// The suspicion window: two periods at full health, the Lifeguard multiplier off (health_max 0) so the
/// aging is exact — a probe that times out reaches Suspect on the resolving tick and Dead two periods on.
fn timing() -> DetectorTiming {
  DetectorTiming {
    suspicion_periods: 2,
    gossip_transmits: 3,
    health_max: 0,
    suspicion_min: 2,
    confirmations_expected: 1,
  }
}

fn budget() -> CommitBudget {
  CommitBudget {
    deadline_ns: DEADLINE_NS,
    poll_interval_ns: POLL_NS,
  }
}

/// AC (§4.8, boot step 6 live): a silent peer's real probe timeout drives the survivor's detector to
/// declare it dead, and `sync_membership` folds that into a takeover — the survivor retires the peer and
/// takes over the peer's objects that fall to it. Every taken object is one the survivor backed and now
/// owns; the peer is gone from the neighbourhood.
#[test]
fn a_silent_peer_is_detected_dead_and_its_objects_are_taken_over() {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let id = sim.shard_ids()[0];

  let survivor_identity = self_signed(NAME);
  let dead_identity = self_signed(NAME);
  let survivor_cert = survivor_identity.certificate().clone();
  let dead_cert = dead_identity.certificate().clone();

  let (dead_port_tx, dead_port_rx) = channel::<u16>();
  let (survivor_port_tx, survivor_port_rx) = channel::<u16>();
  let (result_tx, result_rx) = channel::<Result<usize, String>>();

  // The dead peer: bind, complete the handshake, then fall silent — it never answers a probe, so the
  // survivor's probe must time out. It stays alive as a task only to keep its socket open for the
  // handshake; a real crashed peer's socket would simply stop responding the same way.
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = dead_port_tx.send(socket.local_addr().unwrap().port());
      let peer_port = recv_port(survivor_port_rx).await;
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, peer_port);
      let mut endpoint =
        Endpoint::server(socket, peer, &dead_identity, &[survivor_cert], FRAME_CAP).unwrap();
      if endpoint.establish().await.is_ok() {
        // Handshaken, now silent: stay alive (socket open) across the survivor's one probe — long enough
        // that the probe times out because no `serve_probe` ever answers — then exit so the sim reaches
        // idle. A real crashed peer's socket stops answering the same way; the survivor's detector then
        // ages the suspicion to death with no further packets. The window covers the probe deadline with
        // margin; the survivor's post-probe tick loop is synchronous, so it needs no sim time.
        slates_rt::futures::sleep(DEADLINE_NS.saturating_mul(4)).await;
      }
    })
    .unwrap();

  // The survivor: an f=1 owner runtime that backs the dead peer's objects. It probes the peer once over
  // the wire (which times out), then drives the detector until the peer is declared dead and folds the
  // view into the fleet, reporting how many objects it took over.
  sim
    .spawn_on(id, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let _ = survivor_port_tx.send(socket.local_addr().unwrap().port());
      let dead_port = recv_port(dead_port_rx).await;
      let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, dead_port);
      let outcome = async {
        let mut endpoint = Endpoint::client(
          socket,
          peer,
          &survivor_identity,
          &dead_cert,
          NAME,
          FRAME_CAP,
        )
        .map_err(|e| format!("{e:?}"))?;
        endpoint.establish().await.map_err(|e| format!("{e:?}"))?;

        // The owner runtime: f=1 with the dead peer as its one neighbour, backing the peer's objects.
        let mut fleet = FleetNode::new(SURVIVOR, Quorum { f: 1 }, &[DEAD]);
        for i in 0..BACKED_OBJECTS {
          fleet.track_object(ObjectId::new(DEAD, i), DEAD);
        }
        let mut detector = Detector::new(SURVIVOR, timing());
        detector.join(DEAD);

        // One real probe over the wire: the silent peer never answers, so this must time out.
        detector.tick();
        let ping = SwimMessage::Ping {
          from: SURVIVOR,
          gossip: detector.gossip(GOSSIP_FANOUT),
        };
        let (_endpoint, probe) = probe_once(endpoint, &ping, budget())
          .await
          .map_err(|e| format!("{e:?}"))?;
        if !matches!(probe, ProbeOutcome::TimedOut) {
          return Err("the silent peer should have timed out".to_owned());
        }

        // Drive the detector: the unanswered probe suspects the peer on the resolving tick, and the
        // suspicion ages to death over the window. Each tick, fold the view into the fleet; when the peer
        // is dead, the fold retires it and returns the objects the survivor takes over.
        for _ in 0..TICK_CEILING {
          detector.tick();
          let takeovers = sync_membership(detector.membership(), &mut fleet);
          if !takeovers.is_empty() {
            // Every taken object is one this node backed and now owns; the dead peer is retired.
            for reassignment in &takeovers {
              if reassignment.new_owner != SURVIVOR {
                return Err("a takeover was not assigned to the survivor".to_owned());
              }
              if fleet.object_owner(reassignment.object) != Some(SURVIVOR) {
                return Err("a taken object is not owned by the survivor".to_owned());
              }
            }
            if fleet.configuration().neighbourhood.contains(&DEAD) {
              return Err("the dead peer was not retired from the neighbourhood".to_owned());
            }
            return Ok(takeovers.len());
          }
        }
        Err("the peer was never declared dead within the tick ceiling".to_owned())
      }
      .await;
      let _ = result_tx.send(outcome);
    })
    .unwrap();

  sim.run_until_idle();

  match result_rx.try_recv() {
    Ok(Ok(taken)) => assert!(
      taken > 0,
      "the survivor took over at least one of the dead peer's objects"
    ),
    other => panic!("the live membership takeover did not complete: {other:?}"),
  }
}
