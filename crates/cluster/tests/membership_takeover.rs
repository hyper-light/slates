//! The live membership → takeover path end to end over the simulated UDP fabric (§4.8, D-14): a survivor
//! probes a peer it backs over a real mutually-authenticated session; the peer is silent (it completes the
//! handshake, then never answers a probe), so the real timeout drives the survivor's failure detector to
//! declare it dead; `sync_membership` folds that converged view into the survivor's `FleetNode` membership,
//! and the configuration the council commits for the takeover (the peer retired, its settled neighbourhood
//! and survivors recorded) names, for each of the peer's objects, the survivor as its successor
//! (`RegionalConfiguration::lineage`), which the survivor's routing then records. This joins the pieces
//! proven separately — the SWIM probe over the transport (`swim.rs`), the detector's suspect→dead aging
//! (the detector's own tests), the detector→membership fold, and the configuration's lineage (the register's
//! own tests) — into the live path a fleet node's control-shard loop runs. The council's own
//! reconcile-and-commit of that retirement over the transport is proven in `config_group_live`. Test by use
//! (R5); real multi-node process deployment is a further gate.

// Test harness: an unwrap here is a failed test.
#![allow(clippy::unwrap_used)]

use std::sync::mpsc::{Receiver, channel};

use rustix::net::{Ipv4Addr, SocketAddrV4};
use rustls::pki_types::PrivateKeyDer;
use slates_cluster::CommitBudget;
use slates_cluster::detector::{Detector, DetectorTiming};
use slates_cluster::fleet::{FleetNode, sync_membership};
use slates_cluster::membership::Liveness;
use slates_cluster::swim::{ProbeOutcome, SwimMessage, probe_once};
use slates_db::register::{Configuration, HostId, Lineage, ObjectId, Quorum};
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::SimRuntime;
use slates_rt::udp::UdpSocket;
use slates_transport::endpoint::Endpoint;
use slates_transport::handshake::Identity;

const NAME: &str = "slates-node";
/// Shape: a small packet budget — sixteen bytes of stream data per frame — so a message spans several
/// packets and the reassembly, credit and loss paths all run.
const FRAME_CAP: usize = slates_transport::session::STREAM_FRAME_HEADER_BYTES + 16;
/// Shape: the receive ceiling the test sessions' windows may auto-tune to — sixty-four initial windows,
/// room for the tuning path to run without any test holding more than a few kilobytes.
const RECEIVE_CEILING_WINDOWS: u64 = 64;

/// The connection shape every test session is built with: the frame cap, a ceiling of
/// [`RECEIVE_CEILING_WINDOWS`] initial windows, and the session plane's controller.
fn shape() -> slates_transport::connection::ConnectionShape {
  slates_transport::connection::ConnectionShape::for_frame_cap(
    FRAME_CAP,
    RECEIVE_CEILING_WINDOWS * slates_transport::connection::initial_receive_window(FRAME_CAP),
  )
}
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

async fn recv_port(rx: Receiver<u16>) -> u16 {
  loop {
    if let Ok(p) = rx.try_recv() {
      return p;
    }
    slates_rt::futures::sleep(1_000).await.unwrap();
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
  CommitBudget::hard(DEADLINE_NS, POLL_NS)
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
        Endpoint::server(socket, peer, &dead_identity, &[survivor_cert], shape()).unwrap();
      if endpoint.establish().await.is_ok() {
        // Handshaken, now silent: stay alive (socket open) across the survivor's one probe — long enough
        // that the probe times out because no `serve_probe` ever answers — then exit so the sim reaches
        // idle. A real crashed peer's socket stops answering the same way; the survivor's detector then
        // ages the suspicion to death with no further packets. The window covers the probe deadline with
        // margin; the survivor's post-probe tick loop is synchronous, so it needs no sim time.
        slates_rt::futures::sleep(DEADLINE_NS.saturating_mul(4))
          .await
          .unwrap();
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
        let mut endpoint =
          Endpoint::client(socket, peer, &survivor_identity, &dead_cert, NAME, shape())
            .map_err(|e| format!("{e:?}"))?;
        endpoint.establish().await.map_err(|e| format!("{e:?}"))?;

        // The owner runtime: f=1 with the dead peer as its one neighbour, backing the peer's objects.
        let mut fleet = FleetNode::new(SURVIVOR, Quorum { f: 1 }, &[DEAD]);
        let placement = slates_db::register::RegionalConfiguration::formed(
          vec![SURVIVOR, DEAD],
          Quorum { f: 1 },
          std::collections::BTreeMap::new(),
          u64::try_from(Quorum { f: 1 }.candidates()).unwrap(),
          false,
        );
        for i in 0..BACKED_OBJECTS {
          fleet
            .track_object(ObjectId::new(DEAD, i), DEAD, &placement)
            .unwrap();
        }
        let mut detector = Detector::new(SURVIVOR, timing());
        detector.join(DEAD);

        // One real probe over the wire: the silent peer never answers, so this must time out.
        detector.tick();
        let ping = SwimMessage::Ping {
          from: SURVIVOR,
          nonce: 1,
          boot_nonce: 0,
          configuration_version: 0,
          gossip: detector.gossip(GOSSIP_FANOUT),
        };
        let (_endpoint, probe) = probe_once(endpoint, &ping, budget())
          .await
          .map_err(|e| format!("{e:?}"))?;
        if !matches!(probe, ProbeOutcome::TimedOut) {
          return Err("the silent peer should have timed out".to_owned());
        }

        // Drive the detector: the unanswered probe suspects the peer on the resolving tick, and the
        // suspicion ages to death over the window. Each tick, fold the detector's view into the fleet's
        // membership; once the peer is folded dead, install the configuration the council would commit for
        // its retirement (the region without it) — the survivor then takes over the dead-owned objects that
        // rendezvous first to it. (The council's reconcile-and-commit of that retirement over the transport
        // is proven in `config_group_live`; here the detector→membership→install→takeover path is driven.)
        for _ in 0..TICK_CEILING {
          detector.tick();
          sync_membership(detector.membership(), &mut fleet);
          if fleet.membership().state(DEAD).map(|s| s.liveness) == Some(Liveness::Dead) {
            let mut retired = placement.clone();
            retired.take_over(DEAD, 3);
            let configuration = retired
              .configuration_for(SURVIVOR)
              .unwrap_or_else(|| Configuration::solo(SURVIVOR));
            fleet.install_configuration(configuration, &retired.members);
            let mut taken = 0;
            for i in 0..BACKED_OBJECTS {
              let object = ObjectId::new(DEAD, i);
              match retired.lineage(DEAD, object) {
                Lineage::Successor {
                  departed: DEAD,
                  successor: SURVIVOR,
                } => {
                  fleet.track_object_owner(object, SURVIVOR);
                  taken += 1;
                }
                other => {
                  return Err(format!(
                    "{object:?} was not handed to the survivor: {other:?}"
                  ));
                }
              }
              if fleet.object_owner(object) != Some(SURVIVOR) {
                return Err("a taken object is not owned by the survivor".to_owned());
              }
            }
            if fleet.configuration().neighbourhood.contains(&DEAD) {
              return Err("the dead peer was not retired from the neighbourhood".to_owned());
            }
            return Ok(taken);
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
