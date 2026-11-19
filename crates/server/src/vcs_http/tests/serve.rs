use std::io::{Read, Write as _};
use std::net::TcpStream as ClientStream;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use slates_http::head::Head;
use slates_http::reply::Status;
use slates_http_edge::connection::{Fields, Phase, StatusMode};
use slates_http_edge::tls::ServingIdentity;
use slates_rt::control::Control;
use slates_rt::runtime::{LocalRuntime, RuntimeConfig};
use slates_rt::tcp::{Ipv4Addr, SocketAddrV4, TcpListener};

use super::{
  Answer, Bounds, Counts, Decided, EdgeState, Exchange, Failed, Site, admits_cleartext, serve,
};
use crate::daemon::LIVENESS_BUDGET_NS;

/// Shape: the bytes one client region holds in these tests: room for every request they send.
const ROOM_BYTES: usize = 1 << 20;
/// Shape: one seat: the edge of a shard that seats one client.
const ONE_SEAT: usize = 1;
/// Shape: two seats, one left free while the other is held.
const TWO_SEATS: usize = 2;
/// Shape: the test runtime's timer tick, its `RuntimeConfig::timer_tick_ns` ([`runtime`]).
const TIMER_TICK_NS: u64 = 100_000;
/// Shape: how long a client waits for an answer before it gives up: four liveness windows, past every learned bound these
/// tests reach.
const PATIENCE_NS: u64 = 4 * LIVENESS_BUDGET_NS;
/// Shape: how long a client listens for an answer it must not get: a tenth of a liveness window, far past a loopback
/// round trip and many times a connection's first RTO.
const MOMENT_NS: u64 = LIVENESS_BUDGET_NS / 10;
/// Shape: longer than any learned idle bound these tests reach (one quick idleness's tail plus a liveness window): a
/// liveness window and a half.
const PAST_IDLE_BOUND_NS: u64 = LIVENESS_BUDGET_NS + LIVENESS_BUDGET_NS / 2;
/// Shape: how often a supervising task looks for its client's word: a hundredth of a liveness window.
const TICK_NS: u64 = LIVENESS_BUDGET_NS / 100;
/// Shape: the body of a refused request, past one read of the edge's so the drain has bytes to drop.
const REFUSED_BODY: usize = 1 << 16;
/// Shape: the `Authorization` value that proves a principal to [`Echoes`].
const PROOF: &[u8] = b"Bearer proven";
/// Shape: the end of every chunked reply.
const LAST_CHUNK: &[u8] = b"0\r\n\r\n";
/// Every phase a connection waits on its client in, in [`Seen::learned`]'s order.
const PHASES: [Phase; 6] = [
  Phase::Handshake,
  Phase::Idle,
  Phase::Head,
  Phase::Body,
  Phase::Reply,
  Phase::Drain,
];
/// Shape: the handshake's place in [`PHASES`].
const HANDSHAKE: usize = 0;
/// Shape: idleness's place in [`PHASES`].
const IDLE: usize = 1;

/// A site that serves a request carrying [`PROOF`] (its credentials proved) with an exchange echoing its body, and refuses
/// any other `401`, unproven.
struct Echoes;

impl Site for Echoes {
  type Exchange = Echo;

  async fn decide(&mut self, head: &Head<'_>, _secure: bool) -> Decided<Echo> {
    let proven = head.field(b"authorization").ok().flatten() == Some(PROOF);
    let answer = if proven {
      Answer::Serve {
        fields: Vec::new(),
        head_only: false,
        status: StatusMode::Streamed,
        exchange: Echo::default(),
      }
    } else {
      Answer::Refuse {
        status: Status::UNAUTHORIZED,
        fields: Vec::new(),
      }
    };
    Decided { answer, proven }
  }
}

/// Shape: a body [`Echo`] names [`ECHO_TYPE`] for as its reply begins, failing if its fields are asked again, as
/// Mercurial's exchange fails once its answer is taken.
const TYPED: &[u8] = b"typed";
/// Shape: a body [`Echo`] fails for as its reply begins, `503`.
const FAILS_AS_IT_BEGINS: &[u8] = b"fails";
/// Shape: the media type [`Echo`] names for [`TYPED`].
const ECHO_TYPE: &[u8] = b"text/x-echo";

/// An exchange replying `echo:` and the body it took, in one run; naming its reply's fields, or failing as its reply
/// begins, for the bodies [`TYPED`] and [`FAILS_AS_IT_BEGINS`].
#[derive(Default)]
struct Echo {
  /// The body taken.
  body: Vec<u8>,
  /// Whether it replied.
  replied: bool,
  /// How many times its reply's fields were asked for.
  asked: u32,
}

impl Exchange for Echo {
  async fn body(&mut self, run: &[u8]) -> Result<(), Failed> {
    self.body.extend_from_slice(run);
    Ok(())
  }

  async fn body_end(&mut self) -> Result<(), Failed> {
    Ok(())
  }

  async fn reply_fields(&mut self) -> Result<Fields, Failed> {
    self.asked = self.asked.saturating_add(1);
    match (self.body.as_slice(), self.asked) {
      (TYPED, 1) => Ok(vec![(b"Content-Type".to_vec(), ECHO_TYPE.to_vec())]),
      (TYPED, _) => Err(Failed(Status::INTERNAL_SERVER_ERROR)),
      (FAILS_AS_IT_BEGINS, _) => Err(Failed(Status::SERVICE_UNAVAILABLE)),
      _ => Ok(Fields::new()),
    }
  }

  async fn reply(&mut self, out: &mut Vec<u8>) -> Result<bool, Failed> {
    if self.replied {
      return Ok(false);
    }
    self.replied = true;
    out.extend_from_slice(b"echo:");
    out.extend_from_slice(&self.body);
    Ok(true)
  }
}

/// What a test reads of an edge once its client has finished.
#[derive(Debug)]
struct Seen {
  /// The counts.
  counts: Counts,
  /// The seats still taken.
  serving: usize,
  /// Each phase's completed waits and reaps, in [`PHASES`]' order.
  learned: [(u64, u64); 6],
}

/// What `state` holds now.
fn seen(state: &EdgeState) -> Seen {
  Seen {
    counts: state.counts(),
    serving: state.serving(),
    learned: PHASES.map(|phase| state.learned(phase)),
  }
}

/// Shape: a runtime for one edge: one shard, room for its listener, its connections and their timers.
fn runtime() -> LocalRuntime {
  LocalRuntime::new(&RuntimeConfig {
    shards: 1,
    tasks_per_shard: 32,
    timers_per_shard: 32,
    ring_entries: 8,
    step_budget_ns: 1_000_000,
    timer_tick_ns: TIMER_TICK_NS,
    batch: 8,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
    wake_tracking: None,
  })
  .expect("a local runtime")
}

/// Stops shard `0` when dropped, however its client's thread ends (a failed expectation included), so a test never hangs
/// on a shard nobody stops.
struct StopsShard(u16);

impl Drop for StopsShard {
  fn drop(&mut self) {
    let _ = slates_rt::registry::send_control(self.0, Control::Shutdown);
  }
}

/// A loopback listener for an edge of `bounds`, and its address.
fn listener(bounds: Bounds) -> (TcpListener, SocketAddrV4) {
  let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), bounds.backlog())
    .expect("a loopback listener");
  let address = listener.local_addr().expect("the listener's address");
  (listener, address)
}

/// Serves an edge of `seats` presenting `identity` (cleartext on loopback without one) with [`Echoes`], runs `client`
/// against it on a thread of its own, and stops the shard once the client returns: what the client returned, and what
/// the edge then held.
fn served<C, R>(seats: usize, identity: Option<ServingIdentity>, client: C) -> (R, Seen)
where
  C: FnOnce(SocketAddrV4) -> R + Send + 'static,
  R: Send + 'static,
{
  let runtime = runtime();
  let bounds = Bounds::of_shard(seats, ROOM_BYTES, TIMER_TICK_NS);
  let (listener, address) = listener(bounds);
  let state = runtime
    .context()
    .keep(EdgeState::new(bounds, identity))
    .expect("the edge's state kept on its shard");
  runtime
    .spawn(async move {
      let _ = serve(listener, state, || Echoes).await;
    })
    .expect("the listener's task");
  let shard = runtime.shard_id().0;
  let client = std::thread::spawn(move || {
    let _stops = StopsShard(shard);
    client(address)
  });
  runtime.context().run();
  let result = client.join().expect("the client's thread");
  let seen = state
    .with_in(runtime.context(), seen)
    .expect("the edge's state");
  (result, seen)
}

/// A client's connection to `address`, its reads giving up after [`PATIENCE_NS`].
fn connect(address: SocketAddrV4) -> ClientStream {
  let stream = ClientStream::connect(address).expect("the edge's listener takes the connection");
  stream
    .set_read_timeout(Some(Duration::from_nanos(PATIENCE_NS)))
    .expect("a read timeout");
  stream
}

/// Reads from `stream` until what it read ends with `end`, or the stream ends, fails or times out: everything read.
fn read_until(stream: &mut impl Read, end: &[u8]) -> Vec<u8> {
  let mut read = Vec::new();
  let mut chunk = [0u8; 4096];
  while !read.ends_with(end) {
    match stream.read(&mut chunk) {
      Ok(0) | Err(_) => break,
      Ok(count) => read.extend_from_slice(&chunk[..count]),
    }
  }
  read
}

/// Whether `stream` stays open and silent for `for_ns`: its read times out rather than ending or bringing bytes. Its read
/// timeout is [`PATIENCE_NS`] again after.
fn stays_open(stream: &mut ClientStream, for_ns: u64) -> bool {
  stream
    .set_read_timeout(Some(Duration::from_nanos(for_ns)))
    .expect("a read timeout");
  let open = matches!(
    stream.read(&mut [0u8; 1]),
    Err(error) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
  );
  stream
    .set_read_timeout(Some(Duration::from_nanos(PATIENCE_NS)))
    .expect("the read timeout");
  open
}

/// Whether `stream` ends cleanly: its next read is the end of the stream.
fn ends(stream: &mut ClientStream) -> bool {
  matches!(stream.read(&mut [0u8; 1]), Ok(0))
}

/// Reads until the stream ends: everything read, and whether it ended cleanly rather than failed, reset or timed out.
fn read_to_end(stream: &mut impl Read) -> (Vec<u8>, bool) {
  let mut read = Vec::new();
  let mut chunk = [0u8; 4096];
  loop {
    match stream.read(&mut chunk) {
      Ok(0) => return (read, true),
      Ok(count) => read.extend_from_slice(&chunk[..count]),
      Err(_) => return (read, false),
    }
  }
}

/// A POST of `body`, carrying [`PROOF`] when `proven`.
fn post(body: &[u8], proven: bool) -> Vec<u8> {
  let mut head = format!(
    "POST /echo HTTP/1.1\r\nHost: edge\r\nContent-Length: {}\r\n",
    body.len()
  );
  if proven {
    head.push_str("Authorization: Bearer proven\r\n");
  }
  head.push_str("\r\n");
  [head.as_bytes(), body].concat()
}

/// The reply the edge gives a proven POST of `body`: `200`, chunked, kept.
fn echoed(body: &[u8]) -> Vec<u8> {
  let data = [b"echo:", body].concat();
  let head = format!(
    "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
    data.len()
  );
  [head.as_bytes(), &data, b"\r\n", LAST_CHUNK].concat()
}

/// A self-signed identity for `localhost`, and its certificate for a client to trust.
fn identity() -> (ServingIdentity, CertificateDer<'static>) {
  let key = rcgen::KeyPair::generate().expect("a key");
  let certificate = rcgen::CertificateParams::new(vec!["localhost".to_owned()])
    .expect("parameters")
    .self_signed(&key)
    .expect("a self-signed certificate");
  let der = certificate.der().clone();
  let private = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
  (
    ServingIdentity::new(vec![der.clone()], private).expect("an identity"),
    der,
  )
}

/// A rustls client's connection to `address`, trusting `certificate`.
#[allow(clippy::disallowed_types)] // rustls takes its configuration and provider as `Arc` by signature (D-8's test-harness exception).
fn tls_client(
  address: SocketAddrV4,
  certificate: &CertificateDer<'static>,
) -> StreamOwned<ClientConnection, ClientStream> {
  let mut roots = RootCertStore::empty();
  roots
    .add(certificate.clone())
    .expect("the certificate trusted");
  let provider = std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider());
  let config = ClientConfig::builder_with_provider(provider)
    .with_safe_default_protocol_versions()
    .expect("TLS's versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
  let connection = ClientConnection::new(
    std::sync::Arc::new(config),
    ServerName::try_from("localhost").expect("a server name"),
  )
  .expect("a client connection");
  StreamOwned::new(connection, connect(address))
}

/// A-625 part 2. Do: on one cleartext loopback connection send a proven POST and read its reply, then a second, then
/// close. Expect: both echoed in chunks on the one connection (one accepted), nothing cut or refused, no seat left taken;
/// and the proven connection's waits taught the edge: its idleness and its heads one sample a request (the first folded
/// in at its proof), its reply's writes one each, nothing else.
#[test]
fn a_kept_connection_is_answered_twice_and_its_proven_waits_teach_the_edge() {
  let (replies, seen) = served(ONE_SEAT, None, |address| {
    let mut stream = connect(address);
    stream
      .write_all(&post(b"one", true))
      .expect("the first request");
    let first = read_until(&mut stream, LAST_CHUNK);
    stream.write_all(&post(b"two", true)).expect("the second");
    (first, read_until(&mut stream, LAST_CHUNK))
  });
  assert_eq!(replies, (echoed(b"one"), echoed(b"two")));
  assert_eq!(
    (
      seen.counts.accepted,
      seen.counts.reaped,
      seen.counts.refused,
      seen.counts.failed,
      seen.serving
    ),
    (1, 0, 0, 0, 0)
  );
  assert_eq!(
    seen.learned,
    [(0, 0), (2, 0), (2, 0), (0, 0), (2, 0), (0, 0)]
  );
}

/// A-625 part 2 (the listener at its bound). Do: with one seat, a first client sends a proven POST, reads its reply and
/// keeps its connection open, idle within the bound its request taught the edge (so not overdue); a second client sends a
/// proven POST and listens a moment; then the first closes. Expect: the second accepted but unanswered while the first
/// holds the seat, the listener's wait for a seat counted, and the second answered once the first has closed.
#[test]
fn at_its_bound_the_listener_holds_a_new_connection_until_a_seat_comes_back() {
  let ((first, early, answered), seen) = served(ONE_SEAT, None, |address| {
    let mut holder = connect(address);
    holder
      .write_all(&post(b"first", true))
      .expect("the first request");
    let first = read_until(&mut holder, LAST_CHUNK);
    let mut waiting = connect(address);
    waiting
      .write_all(&post(b"next", true))
      .expect("the request");
    waiting
      .set_read_timeout(Some(Duration::from_nanos(MOMENT_NS)))
      .expect("a short read timeout");
    let early = waiting.read(&mut [0u8; 1]).is_ok_and(|count| count > 0);
    drop(holder);
    waiting
      .set_read_timeout(Some(Duration::from_nanos(PATIENCE_NS)))
      .expect("the read timeout");
    (first, early, read_until(&mut waiting, LAST_CHUNK))
  });
  assert_eq!(
    (first, early, answered),
    (echoed(b"first"), false, echoed(b"next"))
  );
  assert!(
    seen.counts.full >= 1,
    "the listener waited for a seat: {seen:?}"
  );
  assert_eq!(seen.counts.accepted, 2);
}

/// A-625 part 2 (main's ruling, its first side: no cut while seats are free). Do: on an edge of two seats that has learned
/// nothing, connect a client that sends nothing and keep it a moment, many times its first RTO. Expect: the connection
/// still open, though its wait passed its bound and marked it overdue (counted); nothing reaped, nothing learned.
#[test]
fn a_silent_client_is_not_cut_while_seats_are_free() {
  let (open, seen) = served(TWO_SEATS, None, |address| {
    let mut silent = connect(address);
    stays_open(&mut silent, MOMENT_NS)
  });
  assert!(
    open,
    "the silent client kept its seat while another was free"
  );
  assert_eq!((seen.counts.reaped, seen.learned), (0, [(0, 0); 6]));
  assert!(
    seen.counts.overdue >= 1,
    "its wait passed its bound: {seen:?}"
  );
}

/// A-625 part 2 (main's ruling, its second side: a silent client never holds a seat a new connection needs). Do: on an
/// edge of one seat, connect a client that sends nothing and keep it a moment; then a second client sends a proven POST.
/// Expect: the silent client kept through the moment, then reaped for the second, reading its connection's clean end; the
/// second answered; one reap, counted in idleness beside the second's one idle sample.
#[test]
fn a_silent_client_is_reaped_when_a_new_connection_needs_its_seat() {
  let ((kept, answered, reaped), seen) = served(ONE_SEAT, None, |address| {
    let mut silent = connect(address);
    let kept = stays_open(&mut silent, MOMENT_NS);
    let mut needing = connect(address);
    needing
      .write_all(&post(b"needs a seat", true))
      .expect("the request");
    let answered = read_until(&mut needing, LAST_CHUNK);
    (kept, answered, ends(&mut silent))
  });
  assert_eq!(
    (kept, answered, reaped),
    (true, echoed(b"needs a seat"), true)
  );
  assert_eq!((seen.counts.reaped, seen.learned[IDLE]), (1, (1, 1)));
}

/// A-625 part 2 (main's ruling: the most overdue first). Do: on an edge of two seats, connect a silent client and keep it
/// a moment, then a second silent client and keep it a moment; then a third client sends a proven POST. Expect: the first,
/// overdue the longest, reaped for the third, which is answered; the second still open; one reap.
#[test]
fn the_most_overdue_connection_is_reaped_first() {
  let ((first_reaped, answered, second_open), seen) = served(TWO_SEATS, None, |address| {
    let mut first = connect(address);
    let first_kept = stays_open(&mut first, MOMENT_NS);
    let mut second = connect(address);
    let second_kept = stays_open(&mut second, MOMENT_NS);
    let mut needing = connect(address);
    needing
      .write_all(&post(b"third", true))
      .expect("the request");
    let answered = read_until(&mut needing, LAST_CHUNK);
    let second_open = first_kept && second_kept && stays_open(&mut second, MOMENT_NS);
    (ends(&mut first), answered, second_open)
  });
  assert_eq!(
    (first_reaped, answered, second_open),
    (true, echoed(b"third"), true)
  );
  assert_eq!(seen.counts.reaped, 1);
}

/// A-625 part 2 (main's ruling over a learned bound). Do: on an edge of one seat, a client sends a proven POST, reads its
/// reply and stays silent past its learned idle bound (its first idleness's tail plus a liveness window) while no one
/// needs the seat; then a second client sends a proven POST. Expect: the first kept past its bound, then reaped for the
/// second (its clean end read), the second answered; the reap counted, and idleness learned from both clients' first
/// waits beside the one reap.
#[test]
fn a_proven_client_past_its_learned_idle_bound_is_kept_until_its_seat_is_needed() {
  let ((reply, kept, answered, reaped), seen) = served(ONE_SEAT, None, |address| {
    let mut first = connect(address);
    first.write_all(&post(b"once", true)).expect("the request");
    let reply = read_until(&mut first, LAST_CHUNK);
    let kept = stays_open(&mut first, PAST_IDLE_BOUND_NS);
    let mut second = connect(address);
    second
      .write_all(&post(b"twice", true))
      .expect("the request");
    let answered = read_until(&mut second, LAST_CHUNK);
    (reply, kept, answered, ends(&mut first))
  });
  assert_eq!(
    (reply, kept, answered, reaped),
    (echoed(b"once"), true, echoed(b"twice"), true)
  );
  assert_eq!((seen.counts.reaped, seen.learned[IDLE]), (1, (2, 1)));
}

/// A-625 part 2 (RFC 9112 §9.6; A-183's staged close). Do: send a POST without the proof, its body past one of the edge's
/// reads, which the site refuses `401`, and read to the end. Expect: the whole refusal with `Connection: close`, then the
/// connection's clean end: the edge half-closed and drained the body rather than reset it. The refusal counted, nothing
/// learned from a connection that proved no one.
#[test]
fn a_refusal_with_its_body_unread_ends_the_connection_in_stages() {
  let ((reply, clean), seen) = served(ONE_SEAT, None, |address| {
    let mut stream = connect(address);
    stream
      .write_all(&post(&[b'x'; REFUSED_BODY], false))
      .expect("the request");
    read_to_end(&mut stream)
  });
  assert_eq!(
    reply,
    b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
  );
  assert!(clean, "the connection ended cleanly, never reset");
  assert_eq!(seen.counts.refused, 1);
  assert_eq!(seen.learned, [(0, 0); 6]);
}

/// A-625 part 2 (§4.17.2: "TLS is rustls over aws-lc-rs"). Do: over an HTTPS edge, a rustls client completes its handshake
/// and sends a proven POST. Expect: the reply over TLS as over cleartext, and the handshake's wait folded in at the proof:
/// one sample.
#[test]
fn an_https_connection_is_served_and_its_handshake_teaches_the_edge() {
  let (identity, certificate) = identity();
  let (reply, seen) = served(ONE_SEAT, Some(identity), move |address| {
    let mut tls = tls_client(address, &certificate);
    tls.write_all(&post(b"sealed", true)).expect("the request");
    read_until(&mut tls, LAST_CHUNK)
  });
  assert_eq!(reply, echoed(b"sealed"));
  assert_eq!(seen.learned[HANDSHAKE], (1, 0));
}

/// A-625 part 2 (banned item 9: no connection outlives its listener). Do: serve a proven request on a connection that
/// then stays open, and cancel the listener's task. Expect: the connection's task cancelled and joined with its listener:
/// the client reads the connection's clean end, no seat is left taken once the listener's join returns, and nothing was
/// reaped.
#[test]
fn ending_the_listener_cancels_and_joins_its_connections() {
  let runtime = runtime();
  let bounds = Bounds::of_shard(ONE_SEAT, ROOM_BYTES, TIMER_TICK_NS);
  let (listener, address) = listener(bounds);
  let state = runtime
    .context()
    .keep(EdgeState::new(bounds, None))
    .expect("the edge's state kept on its shard");
  let (answered_tx, answered_rx) = std::sync::mpsc::channel::<()>();
  let (joined_tx, joined_rx) = std::sync::mpsc::channel::<usize>();
  runtime
    .spawn(async move {
      let edge = slates_rt::futures::spawn(async move {
        let _ = serve(listener, state, || Echoes).await;
      })
      .expect("the listener's task");
      while answered_rx.try_recv().is_err() {
        let _ = slates_rt::futures::sleep(TICK_NS).await;
      }
      slates_rt::futures::cancel(edge).expect("the listener cancelled");
      let _ = slates_rt::futures::join(edge).await;
      let _ = joined_tx.send(state.with(EdgeState::serving).unwrap_or(usize::MAX));
    })
    .expect("the supervising task");
  let shard = runtime.shard_id().0;
  let client = std::thread::spawn(move || {
    let _stops = StopsShard(shard);
    let mut stream = connect(address);
    stream.write_all(&post(b"held", true)).expect("the request");
    let reply = read_until(&mut stream, LAST_CHUNK);
    answered_tx.send(()).expect("the supervisor listens");
    let (_, ended) = read_to_end(&mut stream);
    let serving = joined_rx
      .recv_timeout(Duration::from_nanos(PATIENCE_NS))
      .ok();
    (reply, ended, serving)
  });
  runtime.context().run();
  let (reply, ended, serving) = client.join().expect("the client's thread");
  assert_eq!((reply, ended, serving), (echoed(b"held"), true, Some(0)));
  let reaped = state.with_in(runtime.context(), |state| state.counts().reaped);
  assert_eq!(reaped, Some(0));
}

/// A-625 part 2 (RFC 7617 §4, RFC 6750 §5.3). Do: ask whether a cleartext edge may serve on loopback addresses and on
/// others. Expect: every loopback address admitted, the unspecified address and a private one refused.
#[test]
fn a_cleartext_edge_serves_on_loopback_alone() {
  let admitted = [
    Ipv4Addr::LOCALHOST,
    Ipv4Addr::new(127, 0, 0, 2),
    Ipv4Addr::UNSPECIFIED,
    Ipv4Addr::new(10, 0, 0, 1),
  ]
  .map(|ip| admits_cleartext(SocketAddrV4::new(ip, 0)));
  assert_eq!(admitted, [true, true, false, false]);
}

/// A-650 part 1 (the reply's fields named as it begins). Do: on one connection send a proven POST whose exchange names
/// its media type as its reply begins and replies in two runs, failing if its fields are asked again, as Mercurial's
/// exchange fails once its answer is taken; on a second, one whose exchange fails as its reply begins. Expect: the
/// first reply's head carrying the media type after the decision's fields, its fields asked once, the reply whole; the
/// second answered `503` in stages, nothing of a `200` sent, and the connection ended cleanly.
#[test]
fn a_service_names_its_reply_fields_once_and_a_failure_there_is_the_status() {
  let ((typed, failed), _) = served(TWO_SEATS, None, |address| {
    let mut stream = connect(address);
    stream
      .write_all(&post(TYPED, true))
      .expect("the typed request");
    let typed = read_until(&mut stream, LAST_CHUNK);
    drop(stream);
    let mut stream = connect(address);
    stream
      .write_all(&post(FAILS_AS_IT_BEGINS, true))
      .expect("the request that fails");
    (typed, read_to_end(&mut stream))
  });
  let data = [b"echo:", TYPED].concat();
  let head = format!(
    "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
    String::from_utf8_lossy(ECHO_TYPE),
    data.len()
  );
  let whole = [head.as_bytes(), &data, b"\r\n", LAST_CHUNK].concat();
  let refused =
    "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
  assert_eq!(
    (
      String::from_utf8_lossy(&typed),
      String::from_utf8_lossy(&failed.0),
      failed.1
    ),
    (
      String::from_utf8_lossy(&whole),
      String::from_utf8_lossy(refused.as_bytes()),
      true
    )
  );
}
