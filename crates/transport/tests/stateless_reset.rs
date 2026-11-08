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

/// Shape: how long the old dialer waits on its second request before it is dropped: long enough for the request to
/// reach the accepting end, which holds its reply until the dialer is gone.
const SECOND_REQUEST_NS: u64 = 50_000_000;
/// Shape: the pause between the new process's requests to its own peer, so simulated time advances and the old
/// session's retransmissions reach it between them.
const NEW_PROCESS_PAUSE_NS: u64 = 1_000_000;

/// What the accepting end of a session saw once its dialer's port was taken by a new process.
struct Rebound {
  /// The accepting end's wait for its last reply to be acknowledged: its result, or `None` at [`BOUND_NS`].
  settled: Option<String>,
  /// Resets the new process sent.
  resets_sent: u64,
  /// Whether the old dialer held the accepting end's token, and the accepting end the dialer's.
  tokens_held: (bool, bool),
}

/// A dialer holds a session with node `first` and asks a second request, which `first` holds its reply to; the dialer's
/// process ends, and a new process (with the dialer's identity when `same`, else another) takes its port while talking
/// to node `second`; then `first` replies and waits for the acknowledgement. The simulated fabric assigns every socket a
/// fresh port, so the port is a relay's: `first` knows the dialer by the relay's address, and the relay passes `first`'s
/// packets to the old dialer before the restart and to the new process after it, and the new process's answers back.
fn dialer_restarted_onto_its_port(same: bool) -> Rebound {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let shard = sim.shard_ids()[0];
  let mut dialer = identity(2);
  let restarted = dialer.pop().unwrap();
  let original = dialer.pop().unwrap();
  let other = identity(1).pop().unwrap();
  let roster = vec![original.certificate(), other.certificate()];
  let replacement = if same { restarted } else { other };
  let first_node = identity(1).pop().unwrap();
  let second_node = identity(1).pop().unwrap();
  let (first_certificate, second_certificate) =
    (first_node.certificate(), second_node.certificate());
  let (ports_tx, ports_rx) = channel::<(u16, u16)>();
  let (relay_ports_tx, relay_ports_rx) = channel::<u16>();
  let (relay_tx, relay_rx) = channel::<u16>();
  let (old_port_tx, old_port_rx) = channel::<u16>();
  let (new_port_tx, new_port_rx) = channel::<u16>();
  let (asked_tx, asked_rx) = channel::<()>();
  let (gone_tx, gone_rx) = channel::<()>();
  let (settled_tx, settled_rx) = channel::<(Option<String>, bool)>();
  let (finished_tx, finished_rx) = channel::<()>();
  let (relay_done_tx, relay_done_rx) = channel::<()>();
  let (stop_tx, stop_rx) = channel::<()>();
  let (rebound_tx, rebound_rx) = channel::<(u64, bool)>();

  // The nodes: the second serves whatever the new process asks; the first serves the first request, takes the second,
  // replies once the dialer is gone, and waits for the acknowledgement.
  sim
    .spawn_on(shard, async move {
      let first = start_demux(first_node, roster.clone());
      let second = start_demux(second_node, roster);
      let port = |demux: slates_transport::demux::DemuxId| {
        demux.with(Demux::local_addr).unwrap().unwrap().port()
      };
      ports_tx.send((port(first), port(second))).unwrap();
      relay_ports_tx.send(port(first)).unwrap();
      let serving = futures::spawn(async move {
        let mut accepted = within(BOUND_NS, second.accept()).await.unwrap().unwrap();
        within(BOUND_NS, accepted.establish())
          .await
          .unwrap()
          .unwrap();
        while stop_rx.try_recv().is_err() {
          let _ = within(BOUND_NS, accepted.serve_once(|_, request| request)).await;
        }
      })
      .unwrap();
      futures::detach(serving).unwrap();
      let mut accepted = within(BOUND_NS, first.accept()).await.unwrap().unwrap();
      within(BOUND_NS, accepted.establish())
        .await
        .unwrap()
        .unwrap();
      within(BOUND_NS, accepted.serve_once(|_, request| request))
        .await
        .unwrap()
        .unwrap();
      let (stream, _, request) = within(BOUND_NS, accepted.next_request())
        .await
        .unwrap()
        .unwrap();
      asked_tx.send(()).unwrap();
      received(gone_rx).await;
      accepted.reply(stream, &request).unwrap();
      let settled = within(BOUND_NS, accepted.settle())
        .await
        .map(|result| format!("{result:?}"));
      settled_tx
        .send((settled, accepted.holds_reset_token()))
        .unwrap();
      finished_tx.send(()).unwrap();
    })
    .unwrap();

  // The dialer's process, then the one that takes its port.
  sim
    .spawn_on(shard, async move {
      let (_, second) = received(ports_rx).await;
      let relay = SocketAddrV4::new(Ipv4Addr::LOCALHOST, received(relay_rx).await);
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      old_port_tx
        .send(socket.local_addr().unwrap().port())
        .unwrap();
      let mut old =
        Endpoint::client(socket, relay, &original, &first_certificate, NAME, shape()).unwrap();
      within(BOUND_NS, old.establish()).await.unwrap().unwrap();
      within(BOUND_NS, old.request(1, Priority::Control, b"one"))
        .await
        .unwrap()
        .unwrap();
      let _ = within(SECOND_REQUEST_NS, old.request(1, Priority::Control, b"two")).await;
      received(asked_rx).await;
      let held = old.holds_reset_token();
      drop(old);
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      new_port_tx
        .send(socket.local_addr().unwrap().port())
        .unwrap();
      let mut new = Endpoint::client(
        socket,
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, second),
        &replacement,
        &second_certificate,
        NAME,
        shape(),
      )
      .unwrap();
      within(BOUND_NS, new.establish()).await.unwrap().unwrap();
      gone_tx.send(()).unwrap();
      let began = futures::now_ns();
      while finished_rx.try_recv().is_err()
        && futures::now_ns().saturating_sub(began) < BOUND_NS + ROUND_TRIP_NS
      {
        let _ = within(BOUND_NS, new.request(1, Priority::Control, b"new")).await;
        futures::sleep(NEW_PROCESS_PAUSE_NS).await.unwrap();
      }
      let _ = stop_tx.send(());
      let _ = relay_done_tx.send(());
      rebound_tx.send((new.resets_sent(), held)).unwrap();
    })
    .unwrap();

  // The relay: the dialer's port as the first node knows it. Before the restart it passes between the old dialer and
  // the first node; once the new process has a port, the first node's packets go to it and its answers back.
  sim
    .spawn_on(shard, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      relay_tx.send(socket.local_addr().unwrap().port()).unwrap();
      let first = SocketAddrV4::new(Ipv4Addr::LOCALHOST, received(relay_ports_rx).await);
      let old = SocketAddrV4::new(Ipv4Addr::LOCALHOST, received(old_port_rx).await);
      let mut new: Option<SocketAddrV4> = None;
      let mut buffer = [0u8; 2048];
      let mut line: VecDeque<(u64, Vec<u8>, SocketAddrV4)> = VecDeque::new();
      while relay_done_rx.try_recv().is_err() {
        if new.is_none()
          && let Ok(port) = new_port_rx.try_recv()
        {
          new = Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
        }
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
        let to = if from == first {
          new.unwrap_or(old)
        } else if from == old && new.is_none() || Some(from) == new {
          first
        } else {
          continue;
        };
        line.push_back((
          futures::now_ns() + ONE_WAY_NS,
          buffer[..length].to_vec(),
          to,
        ));
      }
    })
    .unwrap();

  sim.run_until_idle();
  let (settled, accepting_held) = settled_rx.try_recv().expect("the first node finished");
  let (resets_sent, dialer_held) = rebound_rx.try_recv().expect("the new process finished");
  Rebound {
    settled,
    resets_sent,
    tokens_held: (dialer_held, accepting_held),
  }
}

/// RFC 9000 §10.3, the supervisor's addition to A-261: any endpoint may answer a packet it cannot associate with a reset,
/// so a process restarted onto the port its old session's dialer held ends that session at the accepting end, which
/// would otherwise retransmit into it until its own timers gave up. Do: hold a session from port P, leave a reply owed,
/// end the dialer, and bind P again with the dialer's identity in a process talking to another node. Expect: both ends
/// had been issued tokens; the new process sent a reset; the accepting end's wait for its reply's acknowledgement ended
/// `PeerReset`.
#[test]
fn a_dialer_restarted_onto_its_port_resets_the_accepting_ends_session() {
  let rebound = dialer_restarted_onto_its_port(true);
  eprintln!(
    "the accepting end's wait ended {:?}; the new process sent {} resets; tokens held {:?}",
    rebound.settled, rebound.resets_sent, rebound.tokens_held
  );
  assert_eq!(
    rebound.tokens_held,
    (true, true),
    "each end was issued its peer's token"
  );
  assert!(rebound.resets_sent >= 1);
  assert_eq!(rebound.settled.as_deref(), Some("Err(PeerReset)"));
}

/// Hostile input on the same path: a process with another identity on that port answers with a reset the accepting end
/// was never issued the token for, and the session is not ended by it. Expect: resets sent, at most one per initial probe
/// timeout (6 in the 4 s bound when measured), and the accepting end still waiting at the bound.
#[test]
fn a_reset_from_another_process_on_the_dialers_port_is_ignored() {
  let rebound = dialer_restarted_onto_its_port(false);
  assert!(
    rebound.resets_sent >= 1,
    "the new process answered the strays"
  );
  // The accepting end retransmitted into the port for the whole bound, and the new process answered at most once per
  // initial probe timeout.
  assert!(
    rebound.resets_sent <= 1 + BOUND_NS / INITIAL_PTO_NS,
    "{} resets in {BOUND_NS} ns",
    rebound.resets_sent
  );
  assert_eq!(
    rebound.settled, None,
    "the accepting end kept waiting on its own timers"
  );
}

/// Shape: the strays sent from a dialer's address while its session is still in its handshake.
const HANDSHAKE_STRAYS: usize = 8;

/// What the two ends of a handshake saw when strays came from the dialer's address before it completed.
struct StrayedHandshake {
  /// The dialer's and the accepting end's handshake results.
  outcomes: (String, String),
  /// Datagrams the accepting end discarded.
  discarded: u64,
  /// The demultiplexer's counters once both ends were done.
  counters: DemuxCounters,
}

/// A dialer's handshake through a relay that, once it has passed the dialer's first flight, sends the accepting node
/// [`HANDSHAKE_STRAYS`] short-header packets from the same address, each shaped as a reset with a plausible tail (one of
/// the dialer's own tokens, for a session that does not exist).
fn handshake_with_strays_from_its_dialer() -> StrayedHandshake {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let shard = sim.shard_ids()[0];
  let node = identity(1).pop().unwrap();
  let node_certificate = node.certificate();
  let dialer = identity(1).pop().unwrap();
  let dialer_certificate = dialer.certificate();
  let stray_token = dialer.reset_key().unwrap().token(&[0xEE; 8]);
  let (node_port_tx, node_port_rx) = channel::<u16>();
  let (relay_tx, relay_rx) = channel::<u16>();
  let (dialer_port_tx, dialer_port_rx) = channel::<u16>();
  let (done_tx, done_rx) = channel::<()>();
  let (dialed_tx, dialed_rx) = channel::<String>();
  let (accepted_tx, accepted_rx) = channel::<(String, u64, DemuxCounters)>();

  sim
    .spawn_on(shard, async move {
      let demux = start_demux(node, vec![dialer_certificate]);
      node_port_tx
        .send(demux.with(Demux::local_addr).unwrap().unwrap().port())
        .unwrap();
      let mut accepted = within(BOUND_NS, demux.accept()).await.unwrap().unwrap();
      let outcome = within(BOUND_NS, accepted.establish()).await;
      // The session binds its id on its first use after the handshake, as every caller's does.
      let _ = accepted.connection_id();
      accepted_tx
        .send((
          format!("{outcome:?}"),
          accepted.discarded(),
          demux.with(Demux::counters).unwrap(),
        ))
        .unwrap();
    })
    .unwrap();
  sim
    .spawn_on(shard, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      dialer_port_tx
        .send(socket.local_addr().unwrap().port())
        .unwrap();
      let relay = SocketAddrV4::new(Ipv4Addr::LOCALHOST, received(relay_rx).await);
      let mut endpoint =
        Endpoint::client(socket, relay, &dialer, &node_certificate, NAME, shape()).unwrap();
      let outcome = within(BOUND_NS, endpoint.establish()).await;
      dialed_tx.send(format!("{outcome:?}")).unwrap();
      done_tx.send(()).unwrap();
    })
    .unwrap();
  sim
    .spawn_on(shard, async move {
      let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
      relay_tx.send(socket.local_addr().unwrap().port()).unwrap();
      let node = SocketAddrV4::new(Ipv4Addr::LOCALHOST, received(node_port_rx).await);
      let dialer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, received(dialer_port_rx).await);
      let mut strayed = false;
      let mut buffer = [0u8; 2048];
      while done_rx.try_recv().is_err() {
        let Some(Ok((length, from))) = within(RELAY_PAUSE_NS, socket.recv_from(&mut buffer)).await
        else {
          continue;
        };
        let to = if from == dialer { node } else { dialer };
        let _ = socket.send_to(&buffer[..length], to);
        if from == dialer && !strayed {
          strayed = true;
          for seed in 0..HANDSHAKE_STRAYS {
            let stray = slates_transport::reset::reset_datagram(&stray_token, STRAY_BYTES, |out| {
              out.fill(u8::try_from(seed).unwrap());
              true
            })
            .unwrap();
            let _ = socket.send_to(&stray, node);
          }
        }
      }
    })
    .unwrap();
  sim.run_until_idle();
  let dialed = dialed_rx.try_recv().expect("the dialer finished");
  let (accepted, discarded, counters) = accepted_rx.try_recv().expect("the node finished");
  StrayedHandshake {
    outcomes: (dialed, accepted),
    discarded,
    counters,
  }
}

/// A-261's guard: a 1-RTT packet naming no session, from a source whose session is still in its handshake, is never
/// handed to that session, nor answered with a reset. Before the handshake binds its id, the session has no token to
/// compare it with; a stray there could only be garbage in its reassembly. Do: hand the accepting node, from the
/// dialer's address and before the handshake completes, short-header packets shaped as resets with a plausible tail.
/// Expect: both ends' handshakes complete; the accepting end discarded nothing (none reached it); the demultiplexer
/// counted every stray as naming no session and sent no reset.
#[test]
fn a_stray_to_a_session_in_its_handshake_leaves_it_unaffected() {
  let strayed = handshake_with_strays_from_its_dialer();
  assert_eq!(
    strayed.outcomes,
    ("Some(Ok(()))".to_owned(), "Some(Ok(()))".to_owned())
  );
  assert_eq!(strayed.discarded, 0, "no stray reached the handshake");
  assert_eq!(
    strayed.counters.unknown_id,
    u64::try_from(HANDSHAKE_STRAYS).unwrap(),
    "{:?}",
    strayed.counters
  );
  assert_eq!(strayed.counters.resets_sent, 0, "{:?}", strayed.counters);
}
