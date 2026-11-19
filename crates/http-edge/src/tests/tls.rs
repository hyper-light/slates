use std::io::{Read as _, Write as _};

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore};

use super::{ALPN_HTTP_1_1, HttpsSession, ServingIdentity, TlsRefused};

/// A self-signed identity for `localhost`, and its certificate for a client to trust.
fn identity() -> (ServingIdentity, CertificateDer<'static>) {
  let key = rcgen::KeyPair::generate().unwrap();
  let cert = rcgen::CertificateParams::new(vec!["localhost".to_owned()])
    .unwrap()
    .self_signed(&key)
    .unwrap();
  let der = cert.der().clone();
  let private = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
  (
    ServingIdentity::new(vec![der.clone()], private).unwrap(),
    der,
  )
}

/// A client trusting `cert`, offering `alpn`, speaking `versions`, nothing read yet.
fn client(
  cert: &CertificateDer<'static>,
  alpn: &[&[u8]],
  versions: &[&'static rustls::SupportedProtocolVersion],
) -> Peer {
  let mut roots = RootCertStore::empty();
  roots.add(cert.clone()).unwrap();
  let provider = std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider());
  let mut config = ClientConfig::builder_with_provider(provider)
    .with_protocol_versions(versions)
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
  config.alpn_protocols = alpn.iter().map(|protocol| protocol.to_vec()).collect();
  Peer {
    connection: ClientConnection::new(
      std::sync::Arc::new(config),
      ServerName::try_from("localhost").unwrap(),
    )
    .unwrap(),
    received: Vec::new(),
    ended: false,
  }
}

/// A client and what it has read: its plaintext, and whether the server's `close_notify` ended it.
struct Peer {
  /// The client's connection.
  connection: ClientConnection,
  /// The plaintext it read.
  received: Vec<u8>,
  /// Whether it read the server's `close_notify`.
  ended: bool,
}

impl Peer {
  /// Takes the server's bytes, processing and reading them as they come, so rustls's buffers never fill.
  fn take(&mut self, mut bytes: &[u8]) {
    while !bytes.is_empty() {
      self.connection.read_tls(&mut bytes).unwrap();
      self.connection.process_new_packets().unwrap();
      self.read_plaintext();
    }
  }

  /// Moves the plaintext the client holds into [`Peer::received`].
  fn read_plaintext(&mut self) {
    let mut chunk = [0u8; 4096];
    loop {
      match self.connection.reader().read(&mut chunk) {
        Ok(0) => {
          self.ended = true;
          return;
        }
        Ok(count) => self.received.extend_from_slice(&chunk[..count]),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return,
        Err(error) => panic!("{error}"),
      }
    }
  }
}

/// Moves bytes both ways until neither side has more to send: the client's to the session and the session's to the
/// client, returning the plaintext the session took.
fn pump(peer: &mut Peer, session: &mut HttpsSession) -> Result<Vec<u8>, TlsRefused> {
  let mut plaintext = Vec::new();
  loop {
    let mut to_server = Vec::new();
    while peer.connection.wants_write() {
      peer.connection.write_tls(&mut to_server).unwrap();
    }
    session.receive(&to_server, &mut plaintext)?;
    let mut to_client = Vec::new();
    session.transmit(&mut to_client)?;
    if to_server.is_empty() && to_client.is_empty() {
      return Ok(plaintext);
    }
    peer.take(&to_client);
  }
}

/// A-625: do shake hands with a client offering `http/1.1` at each TLS version, then send bytes each way. Expect the
/// handshake to end, ALPN to agree on `http/1.1`, and the bytes to come through whole.
#[test]
fn a_client_completes_the_handshake_and_bytes_come_through() {
  let (identity, cert) = identity();
  for version in [&rustls::version::TLS13, &rustls::version::TLS12] {
    let mut session = HttpsSession::new(identity.connection().unwrap());
    let mut client = client(&cert, &[ALPN_HTTP_1_1], &[version]);
    pump(&mut client, &mut session).unwrap();
    assert!(!session.is_handshaking() && !client.connection.is_handshaking());
    assert_eq!(client.connection.alpn_protocol(), Some(ALPN_HTTP_1_1));
    client
      .connection
      .writer()
      .write_all(b"GET / HTTP/1.1\r\n")
      .unwrap();
    assert_eq!(
      pump(&mut client, &mut session).unwrap(),
      b"GET / HTTP/1.1\r\n"
    );
    session.send(b"HTTP/1.1 200 OK\r\n").unwrap();
    pump(&mut client, &mut session).unwrap();
    assert_eq!(client.received, b"HTTP/1.1 200 OK\r\n");
  }
}

/// A-625: do shake hands with a client offering ALPN without `http/1.1`, and with one offering none. Expect the first
/// refused in the handshake and the second served.
#[test]
fn alpn_without_http_1_1_is_refused_and_none_is_served() {
  let (identity, cert) = identity();
  let mut session = HttpsSession::new(identity.connection().unwrap());
  let mut h2_only = client(&cert, &[b"h2"], &[&rustls::version::TLS13]);
  assert!(matches!(
    pump(&mut h2_only, &mut session),
    Err(TlsRefused::Session(_))
  ));
  let mut session = HttpsSession::new(identity.connection().unwrap());
  let mut plain = client(&cert, &[], &[&rustls::version::TLS13]);
  pump(&mut plain, &mut session).unwrap();
  assert!(!session.is_handshaking());
  assert_eq!(plain.connection.alpn_protocol(), None);
}

/// A-625: do send a cleartext request where a `ClientHello` belongs. Expect the session refused, and an alert to send.
#[test]
fn bytes_that_are_not_tls_are_refused() {
  let (identity, _) = identity();
  let mut session = HttpsSession::new(identity.connection().unwrap());
  let mut plaintext = Vec::new();
  assert!(matches!(
    session.receive(b"GET / HTTP/1.1\r\nHost: h\r\n\r\n", &mut plaintext),
    Err(TlsRefused::Session(_))
  ));
  assert!(plaintext.is_empty());
  let mut alert = Vec::new();
  session.transmit(&mut alert).unwrap();
  assert!(!alert.is_empty());
}

/// A-625: do send a reply four times rustls's 64 KiB of unsent records. Expect it sealed whole and received whole.
#[test]
fn a_reply_past_rustls_buffer_is_sealed_whole() {
  let (identity, cert) = identity();
  let mut session = HttpsSession::new(identity.connection().unwrap());
  let mut client = client(&cert, &[ALPN_HTTP_1_1], &[&rustls::version::TLS13]);
  pump(&mut client, &mut session).unwrap();
  let reply: Vec<u8> = (0..(256u32 << 10)).map(|at| at.to_le_bytes()[0]).collect();
  session.send(&reply).unwrap();
  pump(&mut client, &mut session).unwrap();
  assert_eq!(client.received, reply);
}

/// A-625: do close the session, and have the client close. Expect the client to read the server's `close_notify` as
/// the end of its stream, and the session to see the client's.
#[test]
fn close_notify_ends_each_side_cleanly() {
  let (identity, cert) = identity();
  let mut session = HttpsSession::new(identity.connection().unwrap());
  let mut client = client(&cert, &[ALPN_HTTP_1_1], &[&rustls::version::TLS13]);
  pump(&mut client, &mut session).unwrap();
  session.close().unwrap();
  pump(&mut client, &mut session).unwrap();
  assert!(client.ended);
  assert!(!session.closed_by_peer());
  client.connection.send_close_notify();
  pump(&mut client, &mut session).unwrap();
  assert!(session.closed_by_peer());
}
