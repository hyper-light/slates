//! Stateless reset (A-261; RFC 9000 §10.3) over the simulated UDP fabric. A dialer holds an established session with
//! an accepting node behind a relay; the node "restarts": the relay starts passing the dialer's packets to a second
//! demultiplexer that never held the session, as a daemon restarted on the same port would be. With the same identity
//! the second node recomputes the session's token, answers the dialer's first packet with a reset, and the dialer's
//! session ends a round trip later. With another identity its reset ends in a token the dialer was never issued, and
//! the dialer ignores it.

#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::collections::VecDeque;
use std::future::Future;
use std::sync::mpsc::{Receiver, channel};

use rustls::pki_types::PrivateKeyDer;
use slates_rt::futures;
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::SimRuntime;
use slates_rt::udp::{Ipv4Addr, SocketAddrV4, UdpSocket};
use slates_transport::connection::Priority;
use slates_transport::demux::{Demux, DemuxCounters};
use slates_transport::endpoint::Endpoint;
use slates_transport::handshake::Identity;

/// Shape: the TLS service name every participant uses.
const NAME: &str = "stateless-reset";
/// Shape: a small packet budget, as the session tests use.
const FRAME_CAP: usize = slates_transport::session::STREAM_FRAME_HEADER_BYTES + 16;
/// Shape: the receive ceiling, in initial windows.
const RECEIVE_CEILING_WINDOWS: u64 = 64;
/// Shape: the relay holds each datagram this long before passing it on (a delay line, in order), so a round trip through it is measurable
/// (twice this) and "within a round trip" means something.
const ONE_WAY_NS: u64 = 5_000_000;
/// Shape: the round trip through the relay.
const ROUND_TRIP_NS: u64 = 2 * ONE_WAY_NS;
/// Shape: the simulation's timer tick (`config`), the resolution every wait is measured at.
const TIMER_TICK_NS: u64 = 100_000;
/// Shape: the bound on any one wait: several initial probe timeouts, so an exchange that completes does so well
/// inside it and one that a defect leaves waiting fails the test rather than hanging it.
const BOUND_NS: u64 = 4_000_000_000;
/// Shape: how long the relay waits for a datagram before checking whether the dialer is done.
const RELAY_PAUSE_NS: u64 = 1_000_000;

fn shape() -> slates_transport::connection::ConnectionShape {
  slates_transport::connection::ConnectionShape::for_frame_cap(
    FRAME_CAP,
    RECEIVE_CEILING_WINDOWS * slates_transport::connection::initial_receive_window(FRAME_CAP),
  )
}

fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 1,
    tasks_per_shard: 64,
    timers_per_shard: 64,
    ring_entries: 64,
    step_budget_ns: 1_000_000_000,
    timer_tick_ns: TIMER_TICK_NS,
    batch: 64,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
    wake_tracking: None,
  }
}

/// `copies` identities of one node: one certificate and one private key, as a daemon restarted under the same identity
/// loads them again.
fn identity(copies: usize) -> Vec<Identity> {
  let key = rcgen::KeyPair::generate().unwrap();
  let certificate = rcgen::CertificateParams::new(vec![NAME.to_owned()])
    .unwrap()
    .self_signed(&key)
    .unwrap();
  let private = PrivateKeyDer::try_from(key.serialize_der()).unwrap();
  (0..copies)
    .map(|_| Identity::from_der(certificate.der().clone(), private.clone_key()))
    .collect()
}

/// Runs `future` to completion or until `within_ns` of simulated time pass (`None`).
async fn within<F: Future>(within_ns: u64, future: F) -> Option<F::Output> {
  let mut future = std::pin::pin!(future);
  let mut deadline = std::pin::pin!(futures::sleep(within_ns));
  std::future::poll_fn(|cx| {
    if let std::task::Poll::Ready(output) = future.as_mut().poll(cx) {
      return std::task::Poll::Ready(Some(output));
    }
    if deadline.as_mut().poll(cx).map(Result::unwrap).is_ready() {
      return std::task::Poll::Ready(None);
    }
    std::task::Poll::Pending
  })
  .await
}

/// Waits, yielding on the runtime, for the one value `rx` will carry.
async fn received<T>(rx: Receiver<T>) -> T {
  loop {
    if let Ok(value) = rx.try_recv() {
      return value;
    }
    futures::sleep(TIMER_TICK_NS).await.unwrap();
  }
}

/// Starts a demultiplexer for `identity` on a fresh port, accepting `allowed`, with its receive loop running.
fn start_demux(
  identity: Identity,
  allowed: Vec<rustls::pki_types::CertificateDer<'static>>,
) -> slates_transport::demux::DemuxId {
  let identity = slates_rt::registry::with_current(|context| context.keep(identity))
    .unwrap()
    .unwrap();
  let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
  let demux = Demux::start(socket, identity, allowed, shape(), 1).unwrap();
  let task = futures::spawn(async move {
    let _ = demux.run().await;
  })
  .unwrap();
  futures::detach(task).unwrap();
  demux
}

/// What comes back after the restart.
enum Behind {
  /// The same node, restarted: the same identity on a demultiplexer that never held the session.
  Restarted,
  /// Another node at the address: another identity.
  AnotherNode,
}

/// What the dialer saw.
struct Outcome {
  /// Whether the dialer held the accepting end's token before the restart (the non-vacuity of the rest).
  held_token: bool,
  /// The dialer's request after the restart: its result, or `None` when it was still waiting at [`BOUND_NS`].
  after: Option<String>,
  /// From the request after the restart being sent to its end.
  took_ns: u64,
  /// The second demultiplexer's counters once the dialer was done.
  behind: DemuxCounters,
}

/// Establishes a session through a relay to one demultiplexer, exchanges a request, then has the relay pass the
/// dialer's packets to a second one, `behind`, and sends another request.
fn restart_behind_relay(behind: Behind) -> Outcome {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let shard = sim.shard_ids()[0];
  let mut node = identity(2);
  let restarted = node.pop().unwrap();
  let original = node.pop().unwrap();
  let accepting_certificate = original.certificate();
  let dialer_identity = identity(1).pop().unwrap();
  let dialer_certificate = dialer_identity.certificate();
  let replacement = match behind {
    Behind::Restarted => restarted,
    Behind::AnotherNode => identity(1).pop().unwrap(),
  };
  let (ports_tx, ports_rx) = channel::<(u16, u16)>();
  let (relay_tx, relay_rx) = channel::<u16>();
  let (dialer_port_tx, dialer_port_rx) = channel::<u16>();
  let (restart_tx, restart_rx) = channel::<()>();
  let (dialer_done_tx, dialer_done_rx) = channel::<()>();
  let (nodes_done_tx, nodes_done_rx) = channel::<()>();
  let (outcome_tx, outcome_rx) = channel::<(bool, Option<String>, u64)>();
  let (counters_tx, counters_rx) = channel::<DemuxCounters>();

  // The two nodes: the first accepts the session and serves one request; the second only runs, as a node that has
  // just come back up.
  sim
    .spawn_on(shard, async move {
      let first = start_demux(original, vec![dialer_certificate.clone()]);
      let second = start_demux(replacement, vec![dialer_certificate]);
      let port = |demux: slates_transport::demux::DemuxId| {
        demux.with(Demux::local_addr).unwrap().unwrap().port()
      };
      ports_tx.send((port(first), port(second))).unwrap();
      let mut accepted = within(BOUND_NS, first.accept()).await.unwrap().unwrap();
      within(BOUND_NS, accepted.establish())
        .await
        .unwrap()
        .unwrap();
      within(BOUND_NS, accepted.serve_once(|_, request| request))
        .await
        .unwrap()
        .unwrap();
      received(nodes_done_rx).await;
      counters_tx
        .send(second.with(Demux::counters).unwrap())
        .unwrap();
      // The first node's session is held to the end: it is the process that died, gone silent, not one that forgot
      // the session and could reset it itself.
      drop(accepted);
    })
    .unwrap();

  // The dialer.
  sim
    .spawn_on(shard, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      dialer_port_tx
        .send(socket.local_addr().unwrap().port())
        .unwrap();
      let relay = SocketAddrV4::new(Ipv4Addr::LOCALHOST, received(relay_rx).await);
      let mut dialer = Endpoint::client(
        socket,
        relay,
        &dialer_identity,
        &accepting_certificate,
        NAME,
        shape(),
      )
      .unwrap();
      within(BOUND_NS, dialer.establish()).await.unwrap().unwrap();
      let before = within(BOUND_NS, dialer.request(1, Priority::Control, b"before"))
        .await
        .unwrap()
        .unwrap();
      assert_eq!(before, b"before");
      let held_token = dialer.holds_reset_token();
      restart_tx.send(()).unwrap();
      let began = futures::now_ns();
      let after = within(BOUND_NS, dialer.request(1, Priority::Control, b"after"))
        .await
        .map(|result| format!("{result:?}"));
      let took_ns = futures::now_ns().saturating_sub(began);
      outcome_tx.send((held_token, after, took_ns)).unwrap();
      dialer_done_tx.send(()).unwrap();
    })
    .unwrap();

  // The relay: the node's address as the dialer knows it. Each datagram is held [`ONE_WAY_NS`]; after the restart the
  // dialer's go to the second node, and the first node's are dropped.
  sim
    .spawn_on(shard, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      relay_tx.send(socket.local_addr().unwrap().port()).unwrap();
      let (first, second) = received(ports_rx).await;
      let first = SocketAddrV4::new(Ipv4Addr::LOCALHOST, first);
      let second = SocketAddrV4::new(Ipv4Addr::LOCALHOST, second);
      let dialer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, received(dialer_port_rx).await);
      let mut restarted = false;
      let mut buffer = [0u8; 2048];
      // A delay line: every datagram leaves ONE_WAY_NS after it arrived, in order, however many are on the way.
      let mut line: VecDeque<(u64, Vec<u8>, SocketAddrV4)> = VecDeque::new();
      while dialer_done_rx.try_recv().is_err() {
        let now = futures::now_ns();
        while line.front().is_some_and(|(due, _, _)| *due <= now) {
          let (_, datagram, to) = line.pop_front().unwrap();
          let _ = socket.send_to(&datagram, to);
        }
        let wait = line
          .front()
          .map_or(RELAY_PAUSE_NS, |(due, _, _)| due.saturating_sub(now).max(1));
        let Some(Ok((length, from))) = within(wait, socket.recv_from(&mut buffer)).await else {
          continue;
        };
        restarted = restarted || restart_rx.try_recv().is_ok();
        let to = if from == dialer {
          if restarted { second } else { first }
        } else if from == first && !restarted || from == second {
          dialer
        } else {
          continue;
        };
        line.push_back((
          futures::now_ns() + ONE_WAY_NS,
          buffer[..length].to_vec(),
          to,
        ));
      }
      nodes_done_tx.send(()).unwrap();
    })
    .unwrap();

  sim.run_until_idle();
  let (held_token, after, took_ns) = outcome_rx.try_recv().expect("the dialer finished");
  Outcome {
    held_token,
    after,
    took_ns,
    behind: counters_rx.try_recv().expect("the nodes finished"),
  }
}

/// A-261, the restart this exists for: a node restarted under the same identity answers its dialer's first packet on
/// the old session with a reset, and the dialer's session ends a round trip later, not at its own timers. Do: hold a
/// session through a relay, then have the relay pass the dialer's packets to a node with the same identity that never
/// held it, and send a request. Expect: the dialer held a token before; the request ends `PeerReset` within one round
/// trip through the relay (and a timer tick); the new node sent a reset.
#[test]
fn a_session_with_a_restarted_node_ends_within_a_round_trip() {
  let outcome = restart_behind_relay(Behind::Restarted);
  eprintln!(
    "the dialer's session ended {:?} {} ns after its first packet to the restarted node, a round trip being {} ns; \
     the node's counters {:?}",
    outcome.after, outcome.took_ns, ROUND_TRIP_NS, outcome.behind
  );
  assert!(outcome.held_token, "the dialer was issued a token");
  assert_eq!(outcome.after.as_deref(), Some("Err(PeerReset)"));
  assert!(
    outcome.took_ns <= ROUND_TRIP_NS + TIMER_TICK_NS,
    "ended {} ns after the first packet, a round trip is {ROUND_TRIP_NS} ns",
    outcome.took_ns
  );
  assert!(outcome.behind.resets_sent >= 1, "{:?}", outcome.behind);
}

/// Hostile input, A-261: a reset whose tail is not the token the dialer was issued is ignored, so another node, or
/// anyone who can send to the dialer, cannot end its session. Do: as above, but the node behind the relay has another
/// identity. Expect: it sent resets (so the dialer saw them) and the dialer's request is still waiting at the bound,
/// never `PeerReset`.
#[test]
fn a_reset_from_another_identity_is_ignored() {
  let outcome = restart_behind_relay(Behind::AnotherNode);
  assert!(outcome.held_token, "the dialer was issued a token");
  assert!(outcome.behind.resets_sent >= 1, "{:?}", outcome.behind);
  assert_eq!(
    outcome.after, None,
    "the dialer kept waiting on its own timers"
  );
  assert!(outcome.took_ns >= BOUND_NS);
}

/// Shape: the initial probe timeout, RFC 9002 §6.2.2's 2 × 333 ms: the least time between two resets to one source.
const INITIAL_PTO_NS: u64 = 666_000_000;
/// Shape: a short-header datagram long enough to draw a reset of the full send length (more than 43 bytes).
const STRAY_BYTES: usize = 64;

/// A short-header datagram of `length` bytes naming a connection id no session holds.
fn stray(length: usize) -> Vec<u8> {
  let mut datagram = vec![0x5Au8; length];
  datagram[0] = 0x41;
  datagram
}

/// Hostile input, A-261 and RFC 9000 §10.3.3: what draws a reset and what does not. Do: from a source mid-handshake
/// with the node, send a stray 1-RTT packet; from a stranger, send a stray of 21 bytes, then of 64, a second 64 at once,
/// and a third after the initial probe timeout. Expect: no reset to the source mid-handshake (its packet may only have
/// outrun the binding of its id); none to the 21-byte stray (an answer shorter than it would be below the minimum); the
/// first 64 draws one reset, shorter than its trigger and shaped as a short header; the second 64 draws none; the third
/// draws another.
#[test]
fn a_node_resets_only_a_stranger_at_most_once_per_interval_and_shorter_than_it_sent() {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let shard = sim.shard_ids()[0];
  let (result_tx, result_rx) = channel::<(Vec<usize>, DemuxCounters)>();
  sim
    .spawn_on(shard, async move {
      let node = identity(1).pop().unwrap();
      let dialer = identity(1).pop().unwrap();
      let demux = start_demux(node, vec![dialer.certificate()]);
      let address = demux.with(Demux::local_addr).unwrap().unwrap();
      let handshaking = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let stranger = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      let mut buffer = [0u8; 2048];
      let mut answers = Vec::new();
      // A long-header first byte opens a session for the source, as a dialer's first flight does.
      handshaking
        .send_to(&[0x06u8; STRAY_BYTES], address)
        .unwrap();
      handshaking.send_to(&stray(STRAY_BYTES), address).unwrap();
      // The short trigger goes first, so only the length rule can withhold its answer (after a reset, the interval
      // would withhold it whatever its length).
      for (length, pause) in [
        (MIN_RESET_TRIGGER, 0),
        (STRAY_BYTES, 0),
        (STRAY_BYTES, 0),
        (STRAY_BYTES, INITIAL_PTO_NS),
      ] {
        futures::sleep(pause.max(TIMER_TICK_NS)).await.unwrap();
        stranger.send_to(&stray(length), address).unwrap();
        if let Some(Ok((answer, from))) =
          within(ROUND_TRIP_NS, stranger.recv_from(&mut buffer)).await
        {
          assert_eq!(from, address);
          assert!(
            answer < length,
            "a {answer}-byte reset to a {length}-byte trigger"
          );
          assert_eq!(buffer[0] & 0xC0, 0x40, "shaped as a short header");
          answers.push(answer);
        }
      }
      let heard_by_the_handshaking_source =
        within(TIMER_TICK_NS, handshaking.recv_from(&mut buffer))
          .await
          .is_some_and(|received| received.is_ok_and(|(length, _)| length < STRAY_BYTES));
      assert!(
        !heard_by_the_handshaking_source,
        "no reset to a source with a session here: {:?}",
        demux.with(Demux::counters)
      );
      result_tx
        .send((answers, demux.with(Demux::counters).unwrap()))
        .unwrap();
    })
    .unwrap();
  sim.run_until_idle();
  let (answers, counters) = result_rx.try_recv().expect("the history finished");
  assert_eq!(answers, vec![43, 43], "{counters:?}");
  assert_eq!(counters.resets_sent, 2, "{counters:?}");
  assert_eq!(counters.resets_withheld, 2, "{counters:?}");
  assert_eq!(counters.unknown_id, 5, "{counters:?}");
}

/// Shape: a trigger too short to answer: a reset shorter than it would be 20 bytes, below the 21-byte minimum.
const MIN_RESET_TRIGGER: usize = 21;

/// A-261's first point: the reset key comes from what only the node holds, its private key, never from its
/// certificate, which every peer sees (a key derivable from the certificate would let any peer forge resets). Do: derive
/// an identity's key, and keys from its private key's bytes and from its certificate's. Expect: its tokens are the
/// private key's and not the certificate's.
#[test]
fn the_reset_key_is_the_private_keys_not_the_certificates() {
  let key = rcgen::KeyPair::generate().unwrap();
  let certificate = rcgen::CertificateParams::new(vec![NAME.to_owned()])
    .unwrap()
    .self_signed(&key)
    .unwrap();
  let identity = Identity::from_der(
    certificate.der().clone(),
    PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
  );
  let id = [3u8; 8];
  let token = identity.reset_key().unwrap().token(&id);
  let from = |secret: &[u8]| {
    slates_transport::reset::ResetKey::derive(secret)
      .unwrap()
      .token(&id)
  };
  assert_eq!(token, from(&key.serialize_der()));
  assert_ne!(token, from(certificate.der()));
}
