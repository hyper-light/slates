//! A receive refusal does not end the demultiplexer (§4.10a; the Windows UDP reset's sibling (a), 2026-11-07). The fleet's
//! serve socket loop, `Demux::run`, read every refusal of a receive as the socket's end, so one transient one (an
//! earlier send's ICMP report, which Linux and Windows both report on a socket's next receive) stopped a node's plane
//! for the daemon's life. Histories run over the deterministic UDP fabric, whose fault knob queues a refusal.

#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::future::{Future, poll_fn};
use std::task::Poll;

use rustls::pki_types::PrivateKeyDer;
use slates_rt::futures;
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::SimRuntime;
use slates_rt::udp::{Ipv4Addr, SocketAddrV4, UdpSocket};
use slates_transport::connection::Priority;
use slates_transport::demux::Demux;
use slates_transport::endpoint::Endpoint;
use slates_transport::handshake::Identity;

/// Shape: one TLS service name for all participants.
const NAME: &str = "receive-refusal";
/// Shape: a short request fits in one frame; fragmentation is covered by session.rs.
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
/// Shape: the peers the demultiplexer admits: one dials in each test.
const PEERS: usize = 1;

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

/// Runs `make`'s history on a one-shard simulated runtime until it is idle: whether the history completed.
fn run_history<History: Future<Output = ()> + 'static>(
  make: impl FnOnce() -> History + Send + 'static,
) -> bool {
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
  received.try_recv().is_ok()
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

/// AC (§4.10a): do start a demultiplexer, queue one receive refusal on its socket before its loop's first read, then
/// have a new peer dial it and exchange a request. Expect the session to open and the request answered, and the loop
/// still running. Before 2026-11-07 the loop returned the refusal at its first read, so the peer's handshake was never
/// answered and the history stalled.
#[test]
fn a_refused_receive_does_not_end_the_demultiplexer() {
  let (ended_tx, ended) = std::sync::mpsc::sync_channel(1);
  let completed = run_history(move || async move {
    let server = identities(1).pop().unwrap();
    let client = identities(1).pop().unwrap();
    let certificate = server.certificate();
    let kept = slates_rt::registry::with_current(|context| context.keep(server))
      .unwrap()
      .unwrap();
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = socket.local_addr().unwrap().port();
    let demux = Demux::start(socket, kept, vec![client.certificate()], shape(), PEERS).unwrap();
    slates_rt::sim::sim_udp_refuse_next_recv(
      port,
      slates_rt::error::RtError::DriverRefused {
        call: "recvfrom",
        code: None,
      },
    );
    let task = futures::spawn(async move {
      let outcome = demux.run().await;
      let _ = ended_tx.send(format!("{outcome:?}"));
    })
    .unwrap();
    futures::detach(task).unwrap();
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
    let mut dialer = Endpoint::client(
      socket,
      demux.with(Demux::local_addr).unwrap().unwrap(),
      &client,
      &certificate,
      NAME,
      shape(),
    )
    .unwrap();
    let (dialed, (mut server, accepted)) = together(dialer.establish(), async {
      let mut server = demux.accept().await.unwrap();
      let accepted = server.establish().await;
      (server, accepted)
    })
    .await;
    dialed.expect("the peer's session opens after the refused receive");
    accepted.expect("the demultiplexer accepts the peer after the refused receive");
    let (reply, served) = together(
      dialer.request(1, Priority::Control, b"after the refusal"),
      server.serve_once(|_, request| request.to_vec()),
    )
    .await;
    served.unwrap();
    assert_eq!(reply.unwrap(), b"after the refusal");
  });
  let ended = ended.try_recv().ok();
  assert!(
    completed && ended.is_none(),
    "the peer was served after the refused receive ({completed}) and the loop still runs (it ended: {ended:?})"
  );
}
