//! The network export's RPC-with-TLS connection (RFC 9289; §4.6 "Kubernetes publication without privilege",
//! AUD-29-75), driven by a real rustls client over a real loopback TCP connection against
//! `slates_server::nfs_tls::serve_connection` on a real runtime shard. The oracle is the RFC's own rules: each
//! test names the rule it checks, does it, and expects what the RFC says.

// A test harness: an unwrap is a failed test. rustls's client takes `Arc` by signature (D-8 exception 3, a test
// harness; the owners are this test's client config and rustls's connection).
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::disallowed_types
)]

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::mpsc::channel;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore};
use slates_bridge_nfs::rpc::{AcceptStatus, read_record, reply_bytes, write_record};
use slates_bridge_nfs::rpc_tls::{
  ALPN_SUNRPC, AUTH_TLS, bad_credential_reply, starttls_reply, too_weak_reply,
};
use slates_rt::runtime::{Runtime, RuntimeConfig};
use slates_rt::tcp::{Ipv4Addr, SocketAddrV4, TcpListener};
use slates_transport::handshake::{Identity, rpc_tls_connection};

/// Format: the server's DNS name, in its certificate and in the client's expectation.
const SERVER_NAME: &str = "slates-0.slates.default.svc.cluster.local";
/// Format: the NFS program and version 3, whose `NULL` procedure every test calls inside the session.
const NFS_PROGRAM: u32 = 100_003;
/// Shape: how long a client waits for any one reply before the test fails, never hangs.
const REPLY_WAIT: Duration = Duration::from_secs(10);
/// Shape: the backlog of the test's listener.
const BACKLOG: i32 = 8;

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

/// An authority, and a leaf it issued for `name`, DER.
struct Authority {
  cert: rcgen::Certificate,
  key: rcgen::KeyPair,
}

impl Authority {
  fn new() -> Authority {
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    Authority {
      cert: params.self_signed(&key).unwrap(),
      key,
    }
  }

  fn der(&self) -> CertificateDer<'static> {
    self.cert.der().clone()
  }

  fn issue(&self, name: &str) -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec![name.to_owned()])
      .unwrap()
      .signed_by(&key, &self.cert, &self.key)
      .unwrap();
    (
      cert.der().clone(),
      PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
    )
  }
}

/// A client config trusting `authority` for the server, presenting `client` (if any), offering `alpn` (if any).
fn client_config(
  authority: &Authority,
  client: Option<(CertificateDer<'static>, PrivateKeyDer<'static>)>,
  alpn: Option<&[u8]>,
) -> Arc<ClientConfig> {
  let mut roots = RootCertStore::empty();
  roots.add(authority.der()).unwrap();
  let builder =
    ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
      .with_protocol_versions(&[&rustls::version::TLS13])
      .unwrap()
      .with_root_certificates(roots);
  let mut config = match client {
    Some((cert, key)) => builder.with_client_auth_cert(vec![cert], key).unwrap(),
    None => builder.with_no_client_auth(),
  };
  if let Some(alpn) = alpn {
    config.alpn_protocols = vec![alpn.to_vec()];
  }
  Arc::new(config)
}

/// A call message: xid, CALL, version 2, the program triple, then a credential and an empty `AUTH_NONE` verifier.
fn call(xid: u32, program: u32, version: u32, procedure: u32, credential_flavor: u32) -> Vec<u8> {
  let mut message = Vec::new();
  for word in [
    xid,
    0,
    2,
    program,
    version,
    procedure,
    credential_flavor,
    0,
    0,
    0,
  ] {
    message.extend_from_slice(&word.to_be_bytes());
  }
  message
}

/// The server under test: a one-shard runtime accepting `connections` connections on loopback, each served by
/// `serve_connection` with a session built from `identity`. Returns the runtime and the port.
fn serve(identity: Identity, connections: usize) -> (Runtime, u16) {
  let runtime = Runtime::start(&config()).unwrap();
  let shard = runtime.shard_ids()[0];
  let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), BACKLOG).unwrap();
  let port = listener.local_addr().unwrap().port();
  let (identity_tx, identity_rx) = channel::<Identity>();
  identity_tx.send(identity).unwrap();
  // The connections are served by a shard-local task, as the daemon's are (their futures are not `Send`): the
  // `Send` task handed to the shard spawns it there and lets it run detached.
  runtime
    .spawn_on(shard, async move {
      let identity = identity_rx.recv().unwrap();
      let accept = async move {
        for _ in 0..connections {
          let Ok(stream) = listener.accept().await else {
            return;
          };
          slates_server::nfs_tls::serve_connection(stream, port, || {
            rpc_tls_connection(&identity, ALPN_SUNRPC).ok()
          })
          .await;
        }
      };
      let task = slates_rt::futures::spawn(accept).unwrap();
      let _ = slates_rt::futures::detach(task);
    })
    .unwrap();
  (runtime, port)
}

fn connect(port: u16) -> std::net::TcpStream {
  let stream = std::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
  stream.set_read_timeout(Some(REPLY_WAIT)).unwrap();
  stream
}

/// Sends the `AUTH_TLS` probe and reads its reply's record body.
fn probe(stream: &mut std::net::TcpStream, xid: u32) -> Vec<u8> {
  stream
    .write_all(&write_record(&call(xid, NFS_PROGRAM, 4, 0, AUTH_TLS)))
    .unwrap();
  read_one_record(stream).unwrap()
}

/// Reads one record-marked message from `stream`; `None` when the peer closed first.
fn read_one_record(stream: &mut impl Read) -> Option<Vec<u8>> {
  let mut bytes = Vec::new();
  let mut chunk = [0u8; 4096];
  loop {
    if let Ok((message, _)) = read_record(&bytes) {
      return Some(message);
    }
    match stream.read(&mut chunk) {
      // A signal in this process interrupts a blocking read; the read is retried, as `read_exact` does.
      Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
      Ok(0) | Err(_) => return None,
      Ok(count) => bytes.extend_from_slice(&chunk[..count]),
    }
  }
}

/// Whether the peer closed the connection (end of stream, or a reset) without sending anything more. A read
/// that times out is *not* closed: a server still waiting would otherwise pass for one that refused.
fn closed(stream: &mut std::net::TcpStream) -> bool {
  let mut byte = [0u8; 1];
  loop {
    return match stream.read(&mut byte) {
      // A signal in this process interrupts a blocking read; retried, as `read_exact` does.
      Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
      Ok(0) => true,
      Ok(_) => false,
      Err(e) => !matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
      ),
    };
  }
}

/// The node's identity: a server leaf of `authority`, verifying clients against `authority`.
fn identity(authority: &Authority) -> Identity {
  let (cert, key) = authority.issue(SERVER_NAME);
  Identity::from_der(cert, key).with_authorities(vec![authority.der()])
}

/// RFC 9289 §4.1 and §5. Do: probe, read `STARTTLS`, complete a mutual TLS 1.3 handshake offering `sunrpc`
/// with a client certificate the server's authority issued, and call NFSv3 `NULL` inside the session. Expect:
/// the probe's reply is the RFC's bytes, the session agrees on `sunrpc`, and the call is answered `SUCCESS`
/// inside it, as the loopback listener answers it.
#[test]
fn a_probed_mutually_authenticated_session_serves_calls() {
  let authority = Authority::new();
  let (runtime, port) = serve(identity(&authority), 1);
  let mut socket = connect(port);
  assert_eq!(probe(&mut socket, 7), starttls_reply(7));
  let config = client_config(
    &authority,
    Some(authority.issue("node-a")),
    Some(ALPN_SUNRPC),
  );
  let mut client =
    ClientConnection::new(config, ServerName::try_from(SERVER_NAME).unwrap()).unwrap();
  let mut tls = rustls::Stream::new(&mut client, &mut socket);
  tls
    .write_all(&write_record(&call(9, NFS_PROGRAM, 3, 0, 0)))
    .unwrap();
  let reply = read_one_record(&mut tls).expect("the call is answered inside the session");
  assert_eq!(reply, reply_bytes(9, AcceptStatus::Success, &[]));
  assert_eq!(client.alpn_protocol(), Some(ALPN_SUNRPC));
  drop(socket);
  runtime.shutdown().unwrap();
}

/// A TLS-only listener (§4.6; RFC 9289 §4.1, "depending on local policy"). Do: send an NFS call in cleartext as
/// the connection's first message. Expect: `AUTH_ERROR`/`AUTH_TOOWEAK`, then the connection closed.
#[test]
fn a_cleartext_call_is_refused_too_weak_and_closed() {
  let authority = Authority::new();
  let (runtime, port) = serve(identity(&authority), 1);
  let mut socket = connect(port);
  socket
    .write_all(&write_record(&call(3, NFS_PROGRAM, 3, 0, 1)))
    .unwrap();
  assert_eq!(read_one_record(&mut socket), Some(too_weak_reply(3)));
  assert!(closed(&mut socket));
  runtime.shutdown().unwrap();
}

/// RFC 9289 §4.1: `AUTH_TLS` on a procedure other than `NULL` "MUST" be rejected `AUTH_BADCRED`. Do: send
/// `AUTH_TLS` on procedure 1 as the first call. Expect: `AUTH_BADCRED`, then closed, no TLS.
#[test]
fn auth_tls_on_another_procedure_is_bad_credential() {
  let authority = Authority::new();
  let (runtime, port) = serve(identity(&authority), 1);
  let mut socket = connect(port);
  socket
    .write_all(&write_record(&call(4, NFS_PROGRAM, 4, 1, AUTH_TLS)))
    .unwrap();
  assert_eq!(read_one_record(&mut socket), Some(bad_credential_reply(4)));
  assert!(closed(&mut socket));
  runtime.shutdown().unwrap();
}

/// RFC 9289 §5.1.1: bytes between the probe and the handshake "MUST" be discarded and the connection
/// "SHOULD" be dropped. Do: send the probe and a garbage tail in one write. Expect: the probe is answered,
/// then the connection closes without a TLS session.
#[test]
fn bytes_sent_behind_the_probe_drop_the_connection() {
  let authority = Authority::new();
  let (runtime, port) = serve(identity(&authority), 1);
  let mut socket = connect(port);
  let mut bytes = write_record(&call(5, NFS_PROGRAM, 4, 0, AUTH_TLS));
  bytes.extend_from_slice(b"spurious");
  socket.write_all(&bytes).unwrap();
  assert_eq!(read_one_record(&mut socket), Some(starttls_reply(5)));
  assert!(closed(&mut socket));
  runtime.shutdown().unwrap();
}

/// RFC 9289 §5 ("MUST support certificate-based mutual authentication") and §5.2.1 (path validation). Do: after
/// the probe, handshake with no client certificate, then with one a foreign authority issued. Expect: neither
/// session carries a call: the client's write or read fails.
#[test]
fn a_client_without_a_certificate_from_the_authority_gets_no_session() {
  let authority = Authority::new();
  let foreign = Authority::new();
  let (runtime, port) = serve(identity(&authority), 2);
  for client in [None, Some(foreign.issue("intruder"))] {
    let mut socket = connect(port);
    assert_eq!(probe(&mut socket, 6), starttls_reply(6));
    let config = client_config(&authority, client, Some(ALPN_SUNRPC));
    let mut connection =
      ClientConnection::new(config, ServerName::try_from(SERVER_NAME).unwrap()).unwrap();
    let mut tls = rustls::Stream::new(&mut connection, &mut socket);
    let wrote = tls.write_all(&write_record(&call(10, NFS_PROGRAM, 3, 0, 0)));
    let answered = wrote.is_ok() && read_one_record(&mut tls).is_some();
    assert!(
      !answered,
      "a call was served without an authenticated client"
    );
  }
  runtime.shutdown().unwrap();
}

/// RFC 9289 §5: a client "MUST include" ALPN `sunrpc`. Do: complete the handshake without offering ALPN, then
/// call. Expect: no reply; the server ends the session once the handshake shows no `sunrpc`.
#[test]
fn a_session_without_sunrpc_serves_nothing() {
  let authority = Authority::new();
  let (runtime, port) = serve(identity(&authority), 1);
  let mut socket = connect(port);
  assert_eq!(probe(&mut socket, 8), starttls_reply(8));
  let config = client_config(&authority, Some(authority.issue("node-a")), None);
  let mut connection =
    ClientConnection::new(config, ServerName::try_from(SERVER_NAME).unwrap()).unwrap();
  let mut tls = rustls::Stream::new(&mut connection, &mut socket);
  let wrote = tls.write_all(&write_record(&call(11, NFS_PROGRAM, 3, 0, 0)));
  assert!(
    !(wrote.is_ok() && read_one_record(&mut tls).is_some()),
    "a session without sunrpc served a call"
  );
  runtime.shutdown().unwrap();
}

/// RFC 9289 §4.1: an `AUTH_TLS` probe "within an existing (D)TLS session" "MUST" be rejected `AUTH_BADCRED`. Do:
/// inside an established session, send the probe again, then a plain call. Expect: `AUTH_BADCRED` for the
/// probe, and the plain call still answered (the session continues).
#[test]
fn a_probe_inside_the_session_is_bad_credential() {
  let authority = Authority::new();
  let (runtime, port) = serve(identity(&authority), 1);
  let mut socket = connect(port);
  assert_eq!(probe(&mut socket, 1), starttls_reply(1));
  let config = client_config(
    &authority,
    Some(authority.issue("node-a")),
    Some(ALPN_SUNRPC),
  );
  let mut connection =
    ClientConnection::new(config, ServerName::try_from(SERVER_NAME).unwrap()).unwrap();
  let mut tls = rustls::Stream::new(&mut connection, &mut socket);
  tls
    .write_all(&write_record(&call(12, NFS_PROGRAM, 4, 0, AUTH_TLS)))
    .unwrap();
  assert_eq!(read_one_record(&mut tls), Some(bad_credential_reply(12)));
  tls
    .write_all(&write_record(&call(13, NFS_PROGRAM, 3, 0, 0)))
    .unwrap();
  assert_eq!(
    read_one_record(&mut tls),
    Some(reply_bytes(13, AcceptStatus::Success, &[]))
  );
  drop(socket);
  runtime.shutdown().unwrap();
}
