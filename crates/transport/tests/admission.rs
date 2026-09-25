//! Authenticated session fairness (§4.10a, §4.3): a reconnecting identity may retain its
//! old and replacement endpoints, but cannot consume another peer's authenticated slots.
//! Histories drive TLS and request/reply over the deterministic UDP fabric, holding stale
//! endpoint owners deliberately so release-on-drop cannot conceal an admission defect.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::future::{Future, poll_fn};
use std::task::Poll;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use slates_rt::futures;
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::SimRuntime;
use slates_rt::udp::{Ipv4Addr, SocketAddrV4, UdpSocket};
use slates_transport::demux::{Demux, SessionRefusal};
use slates_transport::endpoint::{Endpoint, EndpointError};
use slates_transport::handshake::Identity;

/// Shape: one TLS service name for all participants.
const NAME: &str = "session-fairness";
/// Shape: a short request fits in one frame; fragmentation is covered by session.rs.
const FRAME_CAP: usize = 16;
/// Shape: two distinct identities compete; the third dial belongs to the first identity.
const PEERS: usize = 2;
/// Shape: the first peer's live endpoint, its replacement, and one excess attempt.
const DIALS: usize = 3;

fn identities(count: usize) -> Vec<Identity> {
  let key = rcgen::KeyPair::generate().unwrap();
  let certificate = rcgen::CertificateParams::new(vec![NAME.to_owned()])
    .unwrap()
    .self_signed(&key)
    .unwrap();
  let private = PrivateKeyDer::try_from(key.serialize_der()).unwrap();
  (0..count)
    .map(|_| Identity::from_der(certificate.der().clone(), private.clone_key()))
    .collect()
}

fn run_history<History: Future<Output = ()> + 'static>(
  make: impl FnOnce() -> History + Send + 'static,
) {
  let config = RuntimeConfig {
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
  };
  let mut sim = SimRuntime::new(&config, 1).unwrap();
  let (done, received) = std::sync::mpsc::sync_channel(1);
  sim
    .spawn_on(sim.shard_ids()[0], async move {
      let task = futures::spawn(async move {
        make().await;
        done.send(()).unwrap();
      })
      .unwrap();
      futures::detach(task).unwrap();
    })
    .unwrap();
  sim.run_until_idle();
  received
    .try_recv()
    .expect("the entire admission history completed");
}

async fn together<Left: Future, Right: Future>(
  left: Left,
  right: Right,
) -> (Left::Output, Right::Output) {
  let mut left = std::pin::pin!(left);
  let mut right = std::pin::pin!(right);
  let (mut left_result, mut right_result) = (None, None);
  poll_fn(|context| {
    if left_result.is_none()
      && let Poll::Ready(result) = left.as_mut().poll(context)
    {
      left_result = Some(result);
    }
    if right_result.is_none()
      && let Poll::Ready(result) = right.as_mut().poll(context)
    {
      right_result = Some(result);
    }
    if left_result.is_some() && right_result.is_some() {
      Poll::Ready((left_result.take().unwrap(), right_result.take().unwrap()))
    } else {
      Poll::Pending
    }
  })
  .await
}

struct Harness {
  demux: &'static Demux,
  certificate: CertificateDer<'static>,
}

impl Harness {
  fn new(allowed: Vec<CertificateDer<'static>>) -> Harness {
    let identity = identities(1).pop().unwrap();
    let certificate = identity.certificate();
    let identity = slates_rt::registry::with_current(|context| context.keep(identity)).unwrap();
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
    let demux = Demux::start(socket, identity, allowed, FRAME_CAP, PEERS).unwrap();
    let task = futures::spawn(async move {
      demux.run().await.unwrap();
    })
    .unwrap();
    futures::detach(task).unwrap();
    Harness { demux, certificate }
  }

  async fn dial(&self, identity: &Identity) -> (Endpoint, Endpoint, Result<(), EndpointError>) {
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
    let mut client = Endpoint::client(
      socket,
      self.demux.local_addr().unwrap(),
      identity,
      &self.certificate,
      NAME,
      FRAME_CAP,
    )
    .unwrap();
    let (dialed, (server, accepted)) = together(client.establish(), async {
      let mut server = self.demux.accept().await;
      let accepted = server.establish().await;
      (server, accepted)
    })
    .await;
    if accepted.is_ok() {
      dialed.expect("the admitted client establishes");
    } else {
      assert!(dialed.is_err(), "a refused server cannot confirm a client");
    }
    (client, server, accepted)
  }

  fn client(&self, identity: &Identity) -> Endpoint {
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
    Endpoint::client(
      socket,
      self.demux.local_addr().unwrap(),
      identity,
      &self.certificate,
      NAME,
      FRAME_CAP,
    )
    .unwrap()
  }

  async fn pending(&self, identity: &Identity) -> (Endpoint, Endpoint) {
    let mut client = self.client(identity);
    {
      let mut handshake = std::pin::pin!(client.establish());
      poll_fn(|context| {
        assert!(handshake.as_mut().poll(context).is_pending());
        Poll::Ready(())
      })
      .await;
    }
    // The client sent a real first flight. Holding the server endpoint here with no handshake poll
    // models a pending serve task; ownership, rather than a timer guess, keeps its reservation full.
    (client, self.demux.accept().await)
  }
}

async fn echo(client: &mut Endpoint, server: &mut Endpoint, bytes: &[u8]) {
  let (reply, served) = together(
    client.request(1, bytes),
    server.serve_once(|_, request| request.to_vec()),
  )
  .await;
  served.unwrap();
  assert_eq!(reply.unwrap(), bytes);
}

/// AC-7.7/§4.10a: hold both generations of A, refuse its third authentication, and serve B.
/// Releasing stale A returns only its own charge; a later A reconnect must work and both live
/// identities must still exchange bytes. All endpoints use the same IP and distinct source ports.
#[test]
fn a_redialing_identity_cannot_take_another_peers_sessions() {
  run_history(|| async {
    let repeat = identities(DIALS);
    let other = identities(1).pop().unwrap();
    let harness = Harness::new(vec![repeat[0].certificate(), other.certificate()]);
    let (old_client, old_server, accepted) = harness.dial(&repeat[0]).await;
    accepted.unwrap();
    let (mut client, mut server, accepted) = harness.dial(&repeat[1]).await;
    accepted.unwrap();
    echo(&mut client, &mut server, b"replacement").await;
    let (excess_client, mut excess_server, refused) = harness.dial(&repeat[2]).await;
    assert!(matches!(
      refused,
      Err(EndpointError::Admission(SessionRefusal::PeerSessions))
    ));
    assert!(
      matches!(
        excess_server.establish().await,
        Err(EndpointError::Admission(SessionRefusal::PeerSessions))
      ),
      "retrying the refused endpoint must recheck admission, never reuse a cached connection id"
    );
    assert_eq!(harness.demux.counters().peer_sessions_refused, 2);
    drop((excess_client, excess_server));
    let (mut other_client, mut other_server, accepted) = harness.dial(&other).await;
    accepted.unwrap();
    echo(&mut other_client, &mut other_server, b"other peer").await;
    echo(&mut client, &mut server, b"A survives").await;
    drop((old_client, old_server));
    let (mut next_client, mut next_server, accepted) = harness.dial(&repeat[2]).await;
    accepted.unwrap();
    drop((client, server));
    echo(&mut next_client, &mut next_server, b"slot reused").await;
    echo(&mut other_client, &mut other_server, b"B survives").await;
    drop((next_client, next_server, other_client, other_server));
    assert_eq!(harness.demux.sessions(), 0, "every owned charge returned");
  });
}

/// AC-7.7/§4.10a: anonymous handshakes fill only their own reservation. An established peer
/// continues exchanging bytes; a new source is refused, then succeeds after one pending owner drops.
#[test]
fn full_pending_capacity_preserves_live_service_and_reclaims_on_drop() {
  run_history(|| async {
    let live = identities(1).pop().unwrap();
    let joining = identities(1).pop().unwrap();
    let harness = Harness::new(vec![live.certificate(), joining.certificate()]);
    let (mut client, mut server, accepted) = harness.dial(&live).await;
    accepted.unwrap();
    let mut pending = Vec::new();
    for _ in 0..PEERS {
      pending.push(harness.pending(&joining).await);
    }
    echo(&mut client, &mut server, b"pending full").await;
    let mut excess = harness.client(&joining);
    assert!(matches!(
      excess.establish().await,
      Err(EndpointError::NotReady)
    ));
    let counters = harness.demux.counters();
    assert!(counters.sessions_refused > 0);
    assert_eq!(counters.peer_sessions_refused, 0);
    assert_eq!(counters.setup_refused, 0);
    assert_eq!(harness.demux.sessions(), PEERS + 1);
    drop(pending.pop());
    let (mut joined_client, mut joined_server, accepted) = harness.dial(&joining).await;
    accepted.unwrap();
    echo(&mut joined_client, &mut joined_server, b"pending freed").await;
    echo(&mut client, &mut server, b"still serving").await;
    drop((pending, client, server, joined_client, joined_server));
    assert_eq!(harness.demux.sessions(), 0);
    assert!(
      harness.demux.counters().high_water <= u64::try_from(harness.demux.capacity()).unwrap()
    );
  });
}

/// AC-7.7/§4.10a: authenticating a new identity cannot exceed the configured peer capacity.
/// The refusal leaves both admitted peers usable; dropping one admits the refused peer on retry.
#[test]
fn a_full_identity_reservation_refuses_a_new_peer_without_replacing_another() {
  run_history(|| async {
    let identities: Vec<Identity> = (0..=PEERS).map(|_| identities(1).pop().unwrap()).collect();
    let harness = Harness::new(identities.iter().map(Identity::certificate).collect());
    let mut held = Vec::new();
    for identity in &identities[..PEERS] {
      let (client, server, accepted) = harness.dial(identity).await;
      accepted.unwrap();
      held.push((client, server));
    }
    let (client, server, refused) = harness.dial(&identities[PEERS]).await;
    assert!(matches!(
      refused,
      Err(EndpointError::Admission(SessionRefusal::PeerCapacity))
    ));
    assert_eq!(harness.demux.counters().peers_refused, 1);
    drop((client, server));
    for (client, server) in &mut held {
      echo(client, server, b"not displaced").await;
    }
    drop(held.pop());
    let (mut client, mut server, accepted) = harness.dial(&identities[PEERS]).await;
    accepted.unwrap();
    echo(&mut client, &mut server, b"identity freed").await;
    drop((held, client, server));
    assert_eq!(harness.demux.sessions(), 0);
  });
}
